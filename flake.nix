
{
  description = "PixelFlow development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          pkg-config
          pipewire.dev
          wayland
          libclang
          glibc.dev
          linuxHeaders
          libxkbcommon
          libX11
          libXcursor
          libXi
          libXrandr
          libxcb
          vulkan-loader
          zsnes
        ];

        LIBCLANG_PATH = "${pkgs.libclang.lib}/lib";

        BINDGEN_EXTRA_CLANG_ARGS =
          "-isystem ${pkgs.glibc.dev}/include -isystem ${pkgs.linuxHeaders}/include";

        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
          pkgs.vulkan-loader
          pkgs.wayland
          pkgs.libxkbcommon
          pkgs.libX11
          pkgs.libXcursor
          pkgs.libXi
          pkgs.libXrandr
          pkgs.libxcb
        ];
      };
    };
}
