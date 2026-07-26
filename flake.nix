{
  description = "Aviary — desktop email, calendar and kanban client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Libraries the binary dlopens at runtime: gpui renders through blade
        # (Vulkan), and the window/font/clipboard stack is resolved lazily. They
        # go on the RPATH of the built binary and into LD_LIBRARY_PATH for the
        # dev shell.
        runtimeLibs = with pkgs; [
          wayland
          libxkbcommon
          libGL
          fontconfig
          freetype
          xorg.libX11
          xorg.libXcursor
          xorg.libXi
          xorg.libXrandr
          xorg.libxcb
          vulkan-loader
        ];

        nativeDeps = with pkgs; [ pkg-config clang wild cmake ];
        buildDeps = runtimeLibs ++ (with pkgs; [ openssl dbus zstd ]);
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "aviary";
          version = "0.1.0";
          src = self;

          # `patches/` pins four crates through [patch.crates-io], so the lock
          # file references paths inside the tree rather than registry hashes.
          cargoLock = {
            lockFile = ./Cargo.lock;
            allowBuiltinFetchGit = true;
          };

          nativeBuildInputs = nativeDeps ++ [ pkgs.makeWrapper ];
          buildInputs = buildDeps;

          RUSTFLAGS = "-C linker=clang -C link-arg=--ld-path=wild";

          # No Google OAuth secret is baked into a Nix build: the sources hold
          # none (see auth/google.rs) and a package built from a public
          # derivation is exactly the case where users bring their own
          # registration through Preferences → Accounts.

          # gpui resolves Vulkan and the Wayland/X11 stack with dlopen, which
          # ignores the RPATH nixpkgs sets from buildInputs.
          postInstall = ''
            wrapProgram $out/bin/aviary \
              --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath runtimeLibs}

            install -Dm644 packaging/aviary.desktop \
              $out/share/applications/aviary.desktop
            install -Dm644 packaging/aviary.svg \
              $out/share/icons/hicolor/scalable/apps/aviary.svg
          '';

          meta = with pkgs.lib; {
            description = "Desktop email, calendar and kanban client for Microsoft 365, Gmail and IMAP/SMTP";
            homepage = "https://github.com/Catvert/aviary";
            license = licenses.asl20;
            platforms = platforms.linux;
            mainProgram = "aviary";
          };
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = nativeDeps;
          buildInputs = buildDeps ++ [ pkgs.wl-clipboard ];

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
          RUSTFLAGS = "-C linker=clang -C link-arg=--ld-path=wild";
        };
      });
}
