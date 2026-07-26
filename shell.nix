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
  # python3 : le script de build de Stylo génère ses tables de propriétés avec
  # Mako et s'arrête net sans interpréteur sur le PATH. Un nix-shell hérite du
  # PATH de l'utilisateur, donc celui du système faisait l'affaire jusqu'ici —
  # sur une machine sans python3, le projet ne compilait pas.
  nativeBuildInputs = with pkgs; [ pkg-config clang wild cmake python3 ];
  buildInputs = runtimeLibs ++ (with pkgs; [ openssl dbus zstd wl-clipboard ]);

  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
  RUSTFLAGS = "-C linker=clang -C link-arg=--ld-path=wild";
}
