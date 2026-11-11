/*
 * PixelFlow interception shim.
 *
 * Loaded into the wrapped program with LD_PRELOAD. It interposes the
 * swap calls a graphics program makes to present a frame
 * (glXSwapBuffers for GLX, eglSwapBuffers for EGL) and, instead of
 * letting the frame reach the program's own window, reads the frame
 * back with glReadPixels and streams it to PixelFlow over a Unix
 * socket named by PIXELFLOW_INTERCEPT_SOCKET. Suppressing the real
 * swap is the replacement: the program's own window never presents
 * the frame, PixelFlow's fullscreen window presents the processed
 * frame instead.
 *
 * When PIXELFLOW_INTERCEPT_SOCKET is unset, or the PixelFlow link is
 * down, every call passes through to the real swap untouched, so a
 * program run outside PixelFlow behaves exactly as it would without
 * the shim.
 *
 * The shim declares the few GL, GLX, and EGL entry points it needs
 * itself instead of including system headers, so it builds on
 * machines without GL development headers. All of them are resolved
 * from the program's own loaded libraries with dlsym(RTLD_NEXT),
 * so the shim links against libc only (glibc 2.34 and later carry
 * dlsym in libc; no -ldl is needed).
 *
 * Frames are top row first, RGB8, tightly packed, matching
 * PixelFlow's Frame layout. OpenGL returns the bottom row first,
 * so the rows are flipped in place before sending.
 */

#define _GNU_SOURCE

#include <dlfcn.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>

/* Minimal GL types and constants used below. */
typedef unsigned int GLenum;
typedef int GLint;
typedef int GLsizei;
typedef unsigned char GLubyte;

#define GL_VIEWPORT 0x0BA2
#define GL_RGB 0x1907
#define GL_UNSIGNED_BYTE 0x1401

/* Minimal GLX types. Display is opaque here; GLXDrawable is the
 * X window id of the program's window, which PixelFlow needs for
 * input forwarding later. */
typedef struct _XDisplay Display;
typedef unsigned long GLXDrawable;

/* Minimal EGL types. EGLDisplay and EGLSurface are opaque handles;
 * the surface handle is reported as the window id, where it is only
 * an opaque token, not an X window. */
typedef void *EGLDisplay;
typedef void *EGLSurface;
typedef int EGLint;
typedef unsigned int EGLBoolean;

#define EGL_WIDTH 0x3057
#define EGL_HEIGHT 0x3056

/* Largest frame the shim reads back. 16384x16384 RGB8 is 768 MiB;
 * anything larger is passed through unintercepted. */
#define MAX_DIMENSION 16384

/* Frame header on the socket: magic, width, height, payload bytes,
 * then the window id, all little-endian, then the RGB8 payload. */
#define HEADER_LEN 24
#define MAGIC "PF01"

/* Environment variable naming the Unix socket to stream frames to. */
#define SOCKET_ENV "PIXELFLOW_INTERCEPT_SOCKET"

typedef void (*glx_swap_fn)(Display *, GLXDrawable);
typedef EGLBoolean (*egl_swap_fn)(EGLDisplay, EGLSurface);
typedef void (*gl_read_pixels_fn)(GLint, GLint, GLsizei, GLsizei, GLenum, GLenum, void *);
typedef void (*gl_get_integerv_fn)(GLenum, GLint *);
typedef EGLBoolean (*egl_query_surface_fn)(EGLDisplay, EGLSurface, EGLint, EGLint *);

static glx_swap_fn real_glx_swap = NULL;
static egl_swap_fn real_egl_swap = NULL;
static gl_read_pixels_fn real_gl_read_pixels = NULL;
static gl_get_integerv_fn real_gl_get_integerv = NULL;
static egl_query_surface_fn real_egl_query_surface = NULL;
static pthread_once_t resolve_once = PTHREAD_ONCE_INIT;

static void resolve_functions(void) {
    real_glx_swap = (glx_swap_fn)dlsym(RTLD_NEXT, "glXSwapBuffers");
    real_egl_swap = (egl_swap_fn)dlsym(RTLD_NEXT, "eglSwapBuffers");
    real_gl_read_pixels = (gl_read_pixels_fn)dlsym(RTLD_NEXT, "glReadPixels");
    real_gl_get_integerv = (gl_get_integerv_fn)dlsym(RTLD_NEXT, "glGetIntegerv");
    real_egl_query_surface = (egl_query_surface_fn)dlsym(RTLD_NEXT, "eglQuerySurface");
}

/* Socket state, guarded by link_mutex: fd is -1 while unconnected,
 * and stays connected once the link is up. */
static pthread_mutex_t link_mutex = PTHREAD_MUTEX_INITIALIZER;
static int link_fd = -1;
static char socket_path[108];

/* Write every byte or fail. Returns 0 on success, -1 on failure. */
static int write_all(int fd, const void *buf, size_t len) {
    const char *p = (const char *)buf;
    while (len > 0) {
        ssize_t n = write(fd, p, len);
        if (n <= 0) {
            return -1;
        }
        p += n;
        len -= (size_t)n;
    }
    return 0;
}

/* Store a 32-bit value little-endian. */
static void put_u32le(unsigned char *dst, uint32_t value) {
    dst[0] = (unsigned char)(value & 0xff);
    dst[1] = (unsigned char)((value >> 8) & 0xff);
    dst[2] = (unsigned char)((value >> 16) & 0xff);
    dst[3] = (unsigned char)((value >> 24) & 0xff);
}

/* Store a 64-bit value little-endian. */
static void put_u64le(unsigned char *dst, uint64_t value) {
    for (int i = 0; i < 8; i++) {
        dst[i] = (unsigned char)((value >> (8 * i)) & 0xff);
    }
}

/* Flip the rows of a tightly packed RGB8 image in place, so the
 * bottom-first order glReadPixels returns becomes top-first. */
static void flip_rows(unsigned char *pixels, uint32_t width, uint32_t height) {
    size_t row_bytes = (size_t)width * 3;
    unsigned char *tmp = (unsigned char *)malloc(row_bytes);
    if (tmp == NULL) {
        return;
    }
    for (uint32_t y = 0; y < height / 2; y++) {
        unsigned char *top = pixels + (size_t)y * row_bytes;
        unsigned char *bottom = pixels + (size_t)(height - 1 - y) * row_bytes;
        memcpy(tmp, top, row_bytes);
        memcpy(top, bottom, row_bytes);
        memcpy(bottom, tmp, row_bytes);
    }
    free(tmp);
}

/*
 * Connect to PixelFlow, once. Returns the connected fd, or -1 when
 * there is no socket configured or the connect fails. A failed
 * connect is retried on the next swap: PixelFlow binds its listener
 * before launching the program, so a failure here means the link is
 * genuinely down, not merely early.
 */
static int link_connect(void) {
    if (link_fd >= 0) {
        return link_fd;
    }
    const char *path = getenv(SOCKET_ENV);
    if (path == NULL || path[0] == '\0' || strlen(path) >= sizeof(socket_path)) {
        return -1;
    }
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        return -1;
    }
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strcpy(addr.sun_path, path);
    if (connect(fd, (struct sockaddr *)&addr, sizeof(addr)) != 0) {
        close(fd);
        return -1;
    }
    link_fd = fd;
    return fd;
}

/*
 * Read the current frame back and stream it to PixelFlow. Returns 1
 * when the frame was delivered, 0 when interception did not happen
 * and the caller must present normally. The window id is the
 * program's GLX drawable, or the opaque EGL surface handle.
 */
static int send_frame(uint32_t width, uint32_t height, uint64_t window_id) {
    if (width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION) {
        return 0;
    }
    if (real_gl_read_pixels == NULL) {
        return 0;
    }
    size_t byte_len = (size_t)width * (size_t)height * 3;
    if (byte_len > UINT32_MAX) {
        return 0;
    }
    unsigned char *pixels = (unsigned char *)malloc(byte_len);
    if (pixels == NULL) {
        return 0;
    }
    real_gl_read_pixels(0, 0, (GLsizei)width, (GLsizei)height, GL_RGB, GL_UNSIGNED_BYTE, pixels);
    flip_rows(pixels, width, height);
    int delivered = 0;
    pthread_mutex_lock(&link_mutex);
    int fd = link_connect();
    if (fd >= 0) {
        unsigned char header[HEADER_LEN];
        memcpy(header, MAGIC, 4);
        put_u32le(header + 4, width);
        put_u32le(header + 8, height);
        put_u32le(header + 12, (uint32_t)byte_len);
        put_u64le(header + 16, window_id);
        if (write_all(fd, header, HEADER_LEN) == 0 &&
            write_all(fd, pixels, byte_len) == 0) {
            delivered = 1;
        } else {
            close(fd);
            link_fd = -1;
        }
    }
    pthread_mutex_unlock(&link_mutex);
    free(pixels);
    return delivered;
}

/* Current drawable size from the GL viewport. Returns 0 on failure. */
static int viewport_size(uint32_t *width, uint32_t *height) {
    if (real_gl_get_integerv == NULL) {
        return 0;
    }
    GLint viewport[4] = {0, 0, 0, 0};
    real_gl_get_integerv(GL_VIEWPORT, viewport);
    if (viewport[2] <= 0 || viewport[3] <= 0) {
        return 0;
    }
    *width = (uint32_t)viewport[2];
    *height = (uint32_t)viewport[3];
    return 1;
}

void glXSwapBuffers(Display *dpy, GLXDrawable drawable) {
    pthread_once(&resolve_once, resolve_functions);
    uint32_t width = 0;
    uint32_t height = 0;
    if (viewport_size(&width, &height) &&
        send_frame(width, height, (uint64_t)drawable) != 0) {
        return;
    }
    if (real_glx_swap != NULL) {
        real_glx_swap(dpy, drawable);
    }
}

EGLBoolean eglSwapBuffers(EGLDisplay display, EGLSurface surface) {
    pthread_once(&resolve_once, resolve_functions);
    uint32_t width = 0;
    uint32_t height = 0;
    if (real_egl_query_surface != NULL &&
        real_egl_query_surface(display, surface, EGL_WIDTH, (EGLint *)&width) != 0 &&
        real_egl_query_surface(display, surface, EGL_HEIGHT, (EGLint *)&height) != 0 &&
        send_frame(width, height, (uint64_t)(uintptr_t)surface) != 0) {
        return 1;
    }
    if (real_egl_swap != NULL) {
        return real_egl_swap(display, surface);
    }
    return 0;
}
