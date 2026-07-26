{ pkgs ? import <nixpkgs> {} }:

let
  runtimeLibs = with pkgs; [
    wayland
    libxkbcommon
    libGL
    fontconfig
    freetype
    libx11
    libxcursor
    libxi
    libxrandr
    libxcb
    # gpui rend via blade (Vulkan)
    vulkan-loader
  ];
in
pkgs.mkShell {
  nativeBuildInputs = with pkgs; [ pkg-config clang wild cmake ];
  buildInputs = runtimeLibs ++ (with pkgs; [ openssl dbus zstd wl-clipboard ]);

  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
  RUSTFLAGS = "-C linker=clang -C link-arg=--ld-path=wild";
}
