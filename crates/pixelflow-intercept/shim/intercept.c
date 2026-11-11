/*
 * PixelFlow interception shim.
 *
 * Loaded into the wrapped program with LD_PRELOAD. It interposes
 * the calls a graphics program makes to present a frame and,
 * instead of letting the frame reach the program's own window,
 * streams it to PixelFlow over a Unix socket named by
 * PIXELFLOW_INTERCEPT_SOCKET. Suppressing the real present is the
 * replacement: the program's own window never presents the frame,
 * PixelFlow's fullscreen window presents the processed frame
 * instead.
 *
 * Two present paths are interposed. GL programs present with a
 * swap call: glXSwapBuffers for GLX, eglSwapBuffers for EGL. On a
 * swap the shim reads the frame back with glReadPixels. Programs
 * that render in software present with X11 image puts:
 * XPutImage and XShmPutImage. On a put the shim converts the
 * image to RGB8 itself, because the pixels are handed to it in
 * the call. A put that covers the whole window is a frame;
 * partial puts accumulate into a shadow copy of the window and
 * the next full put sends the complete frame. Software rendering
 * is the path SDL 1.2 and SDL 2 take for a software surface, and
 * the path zsnes uses in its software video modes, so a shim
 * that only interposes swap calls sees zero frames from such a
 * program. The conversion reads the channels from the image's
 * color masks, and falls back to the default TrueColor visual's
 * masks for the depth when all three masks are zero, which is
 * how SDL 1.2 presents its display image: the X server knows
 * the visual from the drawable, so SDL never sets the masks,
 * and a converter that trusts them reads every channel as zero.
 *
 * When PIXELFLOW_INTERCEPT_SOCKET is unset, or the PixelFlow link
 * is down, every call passes through to the real present
 * untouched, so a program run outside PixelFlow behaves exactly
 * as it would without the shim.
 *
 * The shim declares the few GL, GLX, EGL, and X11 entry points
 * it needs itself instead of including system headers, so it
 * builds on machines without GL or X11 development headers. All
 * of them are resolved from the program's own loaded libraries
 * with dlsym(RTLD_NEXT), so the shim links against libc only
 * (glibc 2.34 and later carry dlsym in libc; no -ldl is needed).
 *
 * The shim is built for the host architecture and, when the
 * toolchain supports it, for 32-bit as well: a 64-bit shared
 * object cannot be preloaded into a 32-bit process, and a 32-bit
 * target such as zsnes needs the 32-bit build. The launcher
 * selects the build matching the target. This source is written
 * to compile for both.
 *
 * Frames are top row first, RGB8, tightly packed, matching
 * PixelFlow's Frame layout. OpenGL returns the bottom row first,
 * so the rows are flipped in place before sending; X11 images
 * are already top row first.
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

/* Minimal X11 types. Display and GC are opaque here; Drawable is
 * the X window id, which PixelFlow needs for input forwarding
 * later. */
typedef struct _XDisplay Display;
typedef void *GC;
typedef unsigned long Drawable;
typedef int Bool;
typedef int Status;

/* XImage layout, through the color masks: the stable Xlib ABI.
 * Only ZPixmap images are intercepted; the format, depth, and
 * masks describe the pixel data the program presents. */
#define ZPixmap 2
#define LSBFirst 0
#define MSBFirst 1
typedef struct _XImage {
    int width;
    int height;
    int xoffset;
    int format;
    char *data;
    int byte_order;
    int bitmap_unit;
    int bitmap_bit_order;
    int bitmap_pad;
    int depth;
    int bytes_per_line;
    int bits_per_pixel;
    unsigned long red_mask;
    unsigned long green_mask;
    unsigned long blue_mask;
    void *obdata;
    void *f;
} XImage;

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
typedef int (*x_put_image_fn)(Display *, Drawable, GC, XImage *, int, int, int, int, unsigned int, unsigned int);
typedef Status (*xshm_put_image_fn)(Display *, Drawable, GC, XImage *, int, int, int, int, unsigned int, unsigned int, Bool);

static glx_swap_fn real_glx_swap = NULL;
static egl_swap_fn real_egl_swap = NULL;
static gl_read_pixels_fn real_gl_read_pixels = NULL;
static gl_get_integerv_fn real_gl_get_integerv = NULL;
static egl_query_surface_fn real_egl_query_surface = NULL;
static x_put_image_fn real_x_put_image = NULL;
static xshm_put_image_fn real_xshm_put_image = NULL;
static pthread_once_t resolve_once = PTHREAD_ONCE_INIT;

static void resolve_functions(void) {
    real_glx_swap = (glx_swap_fn)dlsym(RTLD_NEXT, "glXSwapBuffers");
    real_egl_swap = (egl_swap_fn)dlsym(RTLD_NEXT, "eglSwapBuffers");
    real_gl_read_pixels = (gl_read_pixels_fn)dlsym(RTLD_NEXT, "glReadPixels");
    real_gl_get_integerv = (gl_get_integerv_fn)dlsym(RTLD_NEXT, "glGetIntegerv");
    real_egl_query_surface = (egl_query_surface_fn)dlsym(RTLD_NEXT, "eglQuerySurface");
    real_x_put_image = (x_put_image_fn)dlsym(RTLD_NEXT, "XPutImage");
    real_xshm_put_image = (xshm_put_image_fn)dlsym(RTLD_NEXT, "XShmPutImage");
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
 * Stream one RGB8 frame to PixelFlow. The caller holds
 * link_mutex. Returns 1 when the frame was delivered.
 */
static int send_frame_data_locked(uint32_t width, uint32_t height, const unsigned char *pixels, uint64_t window_id) {
    if (width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION) {
        return 0;
    }
    size_t byte_len = (size_t)width * (size_t)height * 3;
    if (byte_len > UINT32_MAX) {
        return 0;
    }
    int fd = link_connect();
    if (fd < 0) {
        return 0;
    }
    unsigned char header[HEADER_LEN];
    memcpy(header, MAGIC, 4);
    put_u32le(header + 4, width);
    put_u32le(header + 8, height);
    put_u32le(header + 12, (uint32_t)byte_len);
    put_u64le(header + 16, window_id);
    if (write_all(fd, header, HEADER_LEN) == 0 &&
        write_all(fd, pixels, byte_len) == 0) {
        return 1;
    }
    close(fd);
    link_fd = -1;
    return 0;
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
    pthread_mutex_lock(&link_mutex);
    int delivered = send_frame_data_locked(width, height, pixels, window_id);
    pthread_mutex_unlock(&link_mutex);
    free(pixels);
    return delivered;
}

/* Shadow copy of the target window's pixels, RGB8 top-first, plus
 * its dimensions. X11 presents are often partial (dirty
 * rectangles), so partial puts accumulate into the shadow and only
 * a put covering the whole window sends a frame. Guarded by
 * link_mutex, like the socket. */
static unsigned char *shadow_pixels = NULL;
static uint32_t shadow_width = 0;
static uint32_t shadow_height = 0;

/* Shift and bit count of one color mask. TrueColor masks are
 * contiguous. A zero mask has no bits. */
static void mask_shift_bits(unsigned long mask, int *shift, int *bits) {
    if (mask == 0) {
        *shift = 0;
        *bits = 0;
        return;
    }
    int s = 0;
    while (((mask >> s) & 1UL) == 0) {
        s++;
    }
    int b = 0;
    while (((mask >> (s + b)) & 1UL) != 0) {
        b++;
    }
    *shift = s;
    *bits = b;
}

/* Read one pixel value from image data, honoring the image byte
 * order. Only 16- and 32-bit pixels are read. */
static uint32_t read_pixel(const unsigned char *p, int bytes, int byte_order) {
    if (bytes == 2) {
        if (byte_order == MSBFirst) {
            return ((uint32_t)p[0] << 8) | p[1];
        }
        return (uint32_t)p[0] | ((uint32_t)p[1] << 8);
    }
    if (byte_order == MSBFirst) {
        return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
               ((uint32_t)p[2] << 8) | p[3];
    }
    return (uint32_t)p[0] | ((uint32_t)p[1] << 8) |
           ((uint32_t)p[2] << 16) | ((uint32_t)p[3] << 24);
}

/* One 8-bit channel of a pixel value, from its mask. */
static unsigned char channel_value(uint32_t pixel, unsigned long mask, int shift, int bits) {
    if (bits == 0) {
        return 0;
    }
    unsigned long raw = ((unsigned long)pixel & mask) >> shift;
    if (bits >= 8) {
        return (unsigned char)(raw & 0xffUL);
    }
    return (unsigned char)((raw * 255UL) / ((1UL << bits) - 1UL));
}

/* Standard color masks for a ZPixmap image whose masks are unset.
 * SDL 1.2 leaves the masks zero in the display image it presents,
 * so the shim cannot read the channels from the image itself; the
 * default TrueColor visual's masks for the depth are the only
 * sensible reading. Returns 1 when standard masks apply. */
static int standard_masks(int bits_per_pixel, int depth, int byte_order, unsigned long *red, unsigned long *green, unsigned long *blue) {
    if (bits_per_pixel == 32 && (depth == 24 || depth == 32)) {
        if (byte_order == MSBFirst) {
            *red = 0xff000000UL;
            *green = 0x00ff0000UL;
            *blue = 0x0000ff00UL;
        } else {
            *red = 0x00ff0000UL;
            *green = 0x0000ff00UL;
            *blue = 0x000000ffUL;
        }
        return 1;
    }
    if (bits_per_pixel == 16 && depth == 16) {
        *red = 0xf800UL;
        *green = 0x07e0UL;
        *blue = 0x001fUL;
        return 1;
    }
    if (bits_per_pixel == 16 && depth == 15) {
        *red = 0x7c00UL;
        *green = 0x03e0UL;
        *blue = 0x001fUL;
        return 1;
    }
    return 0;
}

/* Convert one source rect of an XImage to RGB8 and copy it into
 * the shadow at (dest_x, dest_y). The caller holds link_mutex and
 * the shadow exists. Rects outside the image or the shadow are
 * clipped, not trusted. Only ZPixmap images with 16- or 32-bit
 * pixels are converted. */
static void accumulate_put_locked(XImage *image, int src_x, int src_y, int dest_x, int dest_y, unsigned int width, unsigned int height) {
    if (image == NULL || image->format != ZPixmap || image->data == NULL) {
        return;
    }
    int bytes = image->bits_per_pixel / 8;
    if (bytes != 2 && bytes != 4) {
        return;
    }
    if (shadow_width == 0 || shadow_height == 0) {
        return;
    }
    if (width > (unsigned int)MAX_DIMENSION || height > (unsigned int)MAX_DIMENSION) {
        return;
    }
    unsigned long red_mask = image->red_mask;
    unsigned long green_mask = image->green_mask;
    unsigned long blue_mask = image->blue_mask;
    if (red_mask == 0 && green_mask == 0 && blue_mask == 0 &&
        !standard_masks(image->bits_per_pixel, image->depth, image->byte_order, &red_mask, &green_mask, &blue_mask)) {
        return;
    }
    int64_t sx0 = src_x;
    int64_t sy0 = src_y;
    int64_t sx1 = (int64_t)src_x + (int64_t)width;
    int64_t sy1 = (int64_t)src_y + (int64_t)height;
    if (sx0 < 0) {
        sx0 = 0;
    }
    if (sy0 < 0) {
        sy0 = 0;
    }
    if (sx1 > image->width) {
        sx1 = image->width;
    }
    if (sy1 > image->height) {
        sy1 = image->height;
    }
    int64_t dx0 = (int64_t)dest_x + (sx0 - (int64_t)src_x);
    int64_t dy0 = (int64_t)dest_y + (sy0 - (int64_t)src_y);
    if (dx0 < 0) {
        sx0 -= dx0;
        dx0 = 0;
    }
    if (dy0 < 0) {
        sy0 -= dy0;
        dy0 = 0;
    }
    if (dx0 + (sx1 - sx0) > (int64_t)shadow_width) {
        sx1 = sx0 + (int64_t)shadow_width - dx0;
    }
    if (dy0 + (sy1 - sy0) > (int64_t)shadow_height) {
        sy1 = sy0 + (int64_t)shadow_height - dy0;
    }
    if (sx0 >= sx1 || sy0 >= sy1) {
        return;
    }
    int rshift, rbits, gshift, gbits, bshift, bbits;
    mask_shift_bits(red_mask, &rshift, &rbits);
    mask_shift_bits(green_mask, &gshift, &gbits);
    mask_shift_bits(blue_mask, &bshift, &bbits);
    for (int64_t y = sy0; y < sy1; y++) {
        for (int64_t x = sx0; x < sx1; x++) {
            const unsigned char *src = (const unsigned char *)image->data +
                (size_t)y * (size_t)image->bytes_per_line + (size_t)x * (size_t)bytes;
            uint32_t pixel = read_pixel(src, bytes, image->byte_order);
            unsigned char *dst = shadow_pixels +
                ((size_t)(dy0 + (y - sy0)) * shadow_width + (size_t)(dx0 + (x - sx0))) * 3;
            dst[0] = channel_value(pixel, red_mask, rshift, rbits);
            dst[1] = channel_value(pixel, green_mask, gshift, gbits);
            dst[2] = channel_value(pixel, blue_mask, bshift, bbits);
        }
    }
}

/* Handle one X11 image put. The caller holds link_mutex. A put
 * covering the whole window resizes the shadow to the image,
 * fills it, and sends it as a frame; a partial put accumulates
 * into the existing shadow and sends nothing. Returns 1 when a
 * frame was delivered and the caller must skip the real call. */
static int handle_put_locked(Drawable drawable, XImage *image, int src_x, int src_y, int dest_x, int dest_y, unsigned int width, unsigned int height) {
    if (image == NULL || image->format != ZPixmap || image->data == NULL) {
        return 0;
    }
    if (width == 0 || height == 0) {
        return 0;
    }
    int full = src_x == 0 && src_y == 0 && dest_x == 0 && dest_y == 0 &&
               width == (unsigned int)image->width &&
               height == (unsigned int)image->height;
    if (!full) {
        if (shadow_width == 0 || shadow_height == 0) {
            return 0;
        }
        accumulate_put_locked(image, src_x, src_y, dest_x, dest_y, width, height);
        return 0;
    }
    if (image->width <= 0 || image->height <= 0 ||
        image->width > MAX_DIMENSION || image->height > MAX_DIMENSION) {
        return 0;
    }
    uint32_t w = (uint32_t)image->width;
    uint32_t h = (uint32_t)image->height;
    if (w != shadow_width || h != shadow_height) {
        unsigned char *next = (unsigned char *)realloc(shadow_pixels, (size_t)w * h * 3);
        if (next == NULL) {
            return 0;
        }
        shadow_pixels = next;
        shadow_width = w;
        shadow_height = h;
    }
    accumulate_put_locked(image, 0, 0, 0, 0, width, height);
    return send_frame_data_locked(shadow_width, shadow_height, shadow_pixels, (uint64_t)drawable);
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

int XPutImage(Display *display, Drawable drawable, GC gc, XImage *image, int src_x, int src_y, int dest_x, int dest_y, unsigned int width, unsigned int height) {
    pthread_once(&resolve_once, resolve_functions);
    pthread_mutex_lock(&link_mutex);
    int delivered = handle_put_locked(drawable, image, src_x, src_y, dest_x, dest_y, width, height);
    pthread_mutex_unlock(&link_mutex);
    if (delivered) {
        return 0;
    }
    if (real_x_put_image != NULL) {
        return real_x_put_image(display, drawable, gc, image, src_x, src_y, dest_x, dest_y, width, height);
    }
    return 0;
}

Status XShmPutImage(Display *display, Drawable drawable, GC gc, XImage *image, int src_x, int src_y, int dest_x, int dest_y, unsigned int width, unsigned int height, Bool send_event) {
    pthread_once(&resolve_once, resolve_functions);
    pthread_mutex_lock(&link_mutex);
    int delivered = handle_put_locked(drawable, image, src_x, src_y, dest_x, dest_y, width, height);
    pthread_mutex_unlock(&link_mutex);
    /* A program that waits for the completion event still needs
     * the real call to generate it, so the frame is delivered and
     * the real call runs too. Without send_event the real call is
     * skipped, so the program's own window never presents. */
    if (delivered && !send_event) {
        return 1;
    }
    if (real_xshm_put_image != NULL) {
        return real_xshm_put_image(display, drawable, gc, image, src_x, src_y, dest_x, dest_y, width, height, send_event);
    }
    return 0;
}
