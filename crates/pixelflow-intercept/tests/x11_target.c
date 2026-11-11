/*
 * x11_target: a real X11 program presenting with XShmPutImage for
 * the interception tests.
 *
 * This is the present path SDL software rendering takes under X11:
 * SDL 1.2's X11 driver turns SDL_Flip and SDL_UpdateRect into
 * XShmPutImage (or XPutImage) calls in
 * src/video/x11/SDL_x11image.c, and SDL 2's UpdateWindowSurface
 * does the same. It is the path a 32-bit program like zsnes
 * presents through in its software video modes, so this target is
 * built 32-bit with -m32 to reproduce the zsnes case exactly:
 * a 32-bit process presenting with X11 image puts.
 *
 * The target opens a 320x240 window, draws an animated pattern
 * into a shared-memory XImage, and presents it with
 * XShmPutImage each frame, falling back to XPutImage when
 * MIT-SHM is unavailable. The animation matches glx_target and
 * sdl_soft_target: a frame-dependent red background (frame i
 * uses red (i*7)%256, green 0, blue 200) and a yellow rectangle
 * sliding right 3 pixels per frame. It draws the frame count
 * given as the first argument, or runs until killed when the
 * count is 0. A second argument of "plain" forces XPutImage
 * instead of XShmPutImage, so the tests can exercise the plain
 * put path directly.
 */

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <X11/extensions/XShm.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <unistd.h>

#define WIDTH 320
#define HEIGHT 240

/* Build one pixel value from 8-bit channels using the visual's
 * color masks, so the target works on any TrueColor visual. */
static unsigned long pixel_from_rgb(unsigned long red_mask,
                                    unsigned long green_mask,
                                    unsigned long blue_mask,
                                    unsigned char r,
                                    unsigned char g,
                                    unsigned char b) {
    unsigned long pixel = 0;
    unsigned long masks[3] = {red_mask, green_mask, blue_mask};
    unsigned char channels[3] = {r, g, b};
    for (int c = 0; c < 3; c++) {
        unsigned long mask = masks[c];
        if (mask == 0) {
            continue;
        }
        int shift = 0;
        while (((mask >> shift) & 1UL) == 0) {
            shift++;
        }
        int bits = 0;
        while (((mask >> (shift + bits)) & 1UL) != 0) {
            bits++;
        }
        unsigned long value = channels[c] >> (8 - bits);
        pixel |= (value << shift) & mask;
    }
    return pixel;
}

int main(int argc, char **argv) {
    int frames = 0;
    int want_shm = 1;
    if (argc > 1) {
        frames = atoi(argv[1]);
    }
    if (argc > 2) {
        want_shm = strcmp(argv[2], "plain") != 0;
    }
    Display *dpy = XOpenDisplay(NULL);
    if (dpy == NULL) {
        fprintf(stderr, "x11_target: cannot open display\n");
        return 1;
    }
    int screen = DefaultScreen(dpy);
    Visual *visual = DefaultVisual(dpy, screen);
    int depth = DefaultDepth(dpy, screen);
    Window win = XCreateSimpleWindow(dpy, RootWindow(dpy, screen), 0, 0,
                                     WIDTH, HEIGHT, 0,
                                     BlackPixel(dpy, screen),
                                     WhitePixel(dpy, screen));
    XMapWindow(dpy, win);
    XStoreName(dpy, win, "x11_target");
    XSync(dpy, False);

    int use_shm = 0;
    XImage *image = NULL;
    XShmSegmentInfo shminfo;
    memset(&shminfo, 0, sizeof(shminfo));
    if (want_shm && XShmQueryExtension(dpy)) {
        image = XShmCreateImage(dpy, visual, depth, ZPixmap, NULL,
                                &shminfo, WIDTH, HEIGHT);
        if (image != NULL) {
            shminfo.shmid = shmget(IPC_PRIVATE,
                                   (size_t)image->bytes_per_line * (size_t)image->height,
                                   IPC_CREAT | 0600);
            if (shminfo.shmid >= 0) {
                shminfo.shmaddr = shmat(shminfo.shmid, NULL, 0);
                if (shminfo.shmaddr != (char *)-1) {
                    image->data = shminfo.shmaddr;
                    shminfo.readOnly = False;
                    if (XShmAttach(dpy, &shminfo)) {
                        use_shm = 1;
                    } else {
                        shmdt(shminfo.shmaddr);
                        shmctl(shminfo.shmid, IPC_RMID, NULL);
                        XDestroyImage(image);
                        image = NULL;
                    }
                } else {
                    shmctl(shminfo.shmid, IPC_RMID, NULL);
                    XDestroyImage(image);
                    image = NULL;
                }
            } else {
                XDestroyImage(image);
                image = NULL;
            }
        }
    }
    if (image == NULL) {
        image = XCreateImage(dpy, visual, depth, ZPixmap, 0, NULL,
                             WIDTH, HEIGHT, 32, 0);
        if (image == NULL) {
            fprintf(stderr, "x11_target: cannot create image\n");
            XCloseDisplay(dpy);
            return 1;
        }
        image->data = (char *)malloc((size_t)image->bytes_per_line *
                                     (size_t)image->height);
        if (image->data == NULL) {
            fprintf(stderr, "x11_target: cannot allocate image\n");
            XDestroyImage(image);
            XCloseDisplay(dpy);
            return 1;
        }
    }

    int i = 0;
    while (frames == 0 || i < frames) {
        unsigned char red = (unsigned char)((i * 7) % 256);
        unsigned long background = pixel_from_rgb(visual->red_mask,
                                                  visual->green_mask,
                                                  visual->blue_mask,
                                                  red, 0, 200);
        unsigned long yellow = pixel_from_rgb(visual->red_mask,
                                              visual->green_mask,
                                              visual->blue_mask,
                                              255, 255, 0);
        int rect_x = (i * 3) % (WIDTH - 80);
        int bytes_per_pixel = image->bits_per_pixel / 8;
        for (int y = 0; y < HEIGHT; y++) {
            for (int x = 0; x < WIDTH; x++) {
                int in_rect = x >= rect_x && x < rect_x + 80 && y >= 30 && y < 90;
                unsigned long pixel = in_rect ? yellow : background;
                char *dst = image->data + (size_t)y * (size_t)image->bytes_per_line +
                            (size_t)x * (size_t)bytes_per_pixel;
                if (bytes_per_pixel == 4) {
                    *(unsigned int *)dst = (unsigned int)pixel;
                } else if (bytes_per_pixel == 2) {
                    *(unsigned short *)dst = (unsigned short)pixel;
                } else {
                    memcpy(dst, &pixel, (size_t)bytes_per_pixel);
                }
            }
        }
        if (use_shm) {
            XShmPutImage(dpy, win, DefaultGC(dpy, screen), image,
                         0, 0, 0, 0, WIDTH, HEIGHT, False);
        } else {
            XPutImage(dpy, win, DefaultGC(dpy, screen), image,
                      0, 0, 0, 0, WIDTH, HEIGHT);
        }
        XSync(dpy, False);
        i++;
        usleep(33000);
    }
    printf("x11_target: drew %d frames (shm=%d)\n", i, use_shm);
    if (use_shm) {
        XShmDetach(dpy, &shminfo);
        shmdt(shminfo.shmaddr);
        shmctl(shminfo.shmid, IPC_RMID, NULL);
    }
    XDestroyImage(image);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    return 0;
}
