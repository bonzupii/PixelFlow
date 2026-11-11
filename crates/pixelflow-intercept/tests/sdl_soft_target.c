/*
 * sdl_soft_target: a real SDL 1.2 software-rendering program for
 * the interception tests.
 *
 * This is the present path zsnes uses in its software video modes
 * (vid_soft in src/linux/sdllink.c): the emulator draws into a
 * 16-bit SDL surface and presents it with SDL_Flip, which the SDL
 * X11 driver turns into XShmPutImage (or XPutImage) calls in
 * src/video/x11/SDL_x11image.c. No GL call happens anywhere in
 * this path, so a shim that only interposes swap calls sees zero
 * frames from it.
 *
 * The target opens a 320x240 16-bit surface, draws 10 frames by
 * default (or the count given as the first argument, 0 meaning run
 * until killed), and exits. Each frame clears the background to a
 * frame-dependent red (frame i uses red (i*7)%256, green 0, blue
 * 200) and paints a yellow rectangle sliding right 3 pixels per
 * frame, matching the glx_target animation so the intercepted
 * pixels are directly comparable.
 */

#include <SDL/SDL.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define WIDTH 320
#define HEIGHT 240

/* Pack an 8-bit per channel color into the surface's 16-bit
 * 565 format. */
static Uint16 pack565(Uint8 r, Uint8 g, Uint8 b) {
    return (Uint16)(((r >> 3) << 11) | ((g >> 2) << 5) | (b >> 3));
}

int main(int argc, char **argv) {
    int frames = 0;
    if (argc > 1) {
        frames = atoi(argv[1]);
    }
    if (SDL_Init(SDL_INIT_VIDEO) != 0) {
        fprintf(stderr, "sdl_soft_target: SDL_Init failed: %s\n", SDL_GetError());
        return 1;
    }
    SDL_Surface *surface =
        SDL_SetVideoMode(WIDTH, HEIGHT, 16, SDL_SWSURFACE);
    if (surface == NULL) {
        fprintf(stderr, "sdl_soft_target: SDL_SetVideoMode failed: %s\n", SDL_GetError());
        SDL_Quit();
        return 1;
    }
    int i = 0;
    while (frames == 0 || i < frames) {
        if (SDL_MUSTLOCK(surface)) {
            SDL_LockSurface(surface);
        }
        Uint16 *pixels = (Uint16 *)surface->pixels;
        int pitch = surface->pitch / 2;
        Uint8 red = (Uint8)((i * 7) % 256);
        Uint16 background = pack565(red, 0, 200);
        Uint16 yellow = pack565(255, 255, 0);
        int rect_x = (i * 3) % (WIDTH - 80);
        for (int y = 0; y < HEIGHT; y++) {
            for (int x = 0; x < WIDTH; x++) {
                int in_rect = x >= rect_x && x < rect_x + 80 && y >= 30 && y < 90;
                pixels[y * pitch + x] = in_rect ? yellow : background;
            }
        }
        if (SDL_MUSTLOCK(surface)) {
            SDL_UnlockSurface(surface);
        }
        SDL_Flip(surface);
        i++;
        usleep(16000);
    }
    printf("sdl_soft_target: drew %d frames\n", i);
    SDL_Quit();
    return 0;
}
