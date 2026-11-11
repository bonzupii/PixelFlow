/*
 * glx_target: a real OpenGL program for the interception tests.
 *
 * Opens a 320x240 GLX window, draws 10 frames, and exits. Each frame
 * clears the background to a frame-dependent red (frame i uses red
 * i*25, green 0, blue 200) and paints a yellow rectangle over it
 * with the scissor test, then swaps. The tests assert the intercepted
 * frames carry exactly these pixels, which proves the frames come
 * from the program's own rendering rather than from a test fixture.
 *
 * GL and GLX entry points are declared here instead of including
 * system GL headers, so the target builds on machines without GL
 * development headers. It links against the runtime libGL.so.1.
 */

#include <X11/Xlib.h>
#include <X11/Xutil.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

typedef unsigned int GLenum;
typedef unsigned int GLbitfield;
typedef unsigned char GLboolean;
typedef float GLclampf;
typedef int GLint;
typedef int GLsizei;

#define GL_COLOR_BUFFER_BIT 0x00004000
#define GL_SCISSOR_TEST 0x0C11

#define GLX_RGBA 4
#define GLX_DOUBLEBUFFER 5
#define GLX_DEPTH_SIZE 12

typedef void *GLXContext;
typedef struct _XDisplay Display;
typedef unsigned long GLXDrawable;

extern XVisualInfo *glXChooseVisual(Display *dpy, int screen, int *attribList);
extern GLXContext glXCreateContext(Display *dpy, XVisualInfo *vis, GLXContext shareList, int direct);
extern int glXMakeCurrent(Display *dpy, GLXDrawable drawable, GLXContext ctx);
extern void glXSwapBuffers(Display *dpy, GLXDrawable drawable);
extern void glXDestroyContext(Display *dpy, GLXContext ctx);
extern void glClearColor(GLclampf red, GLclampf green, GLclampf blue, GLclampf alpha);
extern void glClear(GLbitfield mask);
extern void glViewport(GLint x, GLint y, GLsizei width, GLsizei height);
extern void glEnable(GLenum cap);
extern void glDisable(GLenum cap);
extern void glScissor(GLint x, GLint y, GLsizei width, GLsizei height);

#define WIDTH 320
#define HEIGHT 240
#define FRAMES 10

int main(void) {
    Display *dpy = XOpenDisplay(NULL);
    if (dpy == NULL) {
        fprintf(stderr, "glx_target: cannot open display\n");
        return 1;
    }
    int screen = DefaultScreen(dpy);
    int attrs[] = {GLX_RGBA, GLX_DOUBLEBUFFER, GLX_DEPTH_SIZE, 24, 0};
    XVisualInfo *vi = glXChooseVisual(dpy, screen, attrs);
    if (vi == NULL) {
        fprintf(stderr, "glx_target: no GLX visual\n");
        return 1;
    }
    Colormap cmap = XCreateColormap(dpy, RootWindow(dpy, screen), vi->visual, AllocNone);
    XSetWindowAttributes swa;
    swa.colormap = cmap;
    swa.event_mask = ExposureMask;
    Window win = XCreateWindow(dpy, RootWindow(dpy, screen), 0, 0, WIDTH, HEIGHT, 0,
                               vi->depth, InputOutput, vi->visual,
                               CWColormap | CWEventMask, &swa);
    XMapWindow(dpy, win);
    XStoreName(dpy, win, "glx_target");
    GLXContext ctx = glXCreateContext(dpy, vi, NULL, 1);
    if (ctx == NULL) {
        fprintf(stderr, "glx_target: cannot create context\n");
        return 1;
    }
    if (glXMakeCurrent(dpy, win, ctx) == 0) {
        fprintf(stderr, "glx_target: cannot make context current\n");
        return 1;
    }
    glViewport(0, 0, WIDTH, HEIGHT);
    for (int i = 0; i < FRAMES; i++) {
        float red = (float)(i * 25) / 255.0f;
        glClearColor(red, 0.0f, 200.0f / 255.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        /* Yellow rectangle at GL coordinates x 40..120, y 30..90. */
        glEnable(GL_SCISSOR_TEST);
        glScissor(40, 30, 80, 60);
        glClearColor(1.0f, 1.0f, 0.0f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glDisable(GL_SCISSOR_TEST);
        glXSwapBuffers(dpy, win);
        usleep(16000);
    }
    glXDestroyContext(dpy, ctx);
    XDestroyWindow(dpy, win);
    XCloseDisplay(dpy);
    printf("glx_target: drew %d frames\n", FRAMES);
    return 0;
}
