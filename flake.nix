{
  description = "Aviary — desktop email, calendar and kanban client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  # Binary cache, so `nix run github:Catvert/aviary` does not mean compiling
  # Stylo, Blitz and the whole Rust graph first — an hour on a laptop. CI
  # pushes what it builds from main here.
  #
  # `extra-`, never the bare setting: replacing `substituters` would drop
  # cache.nixos.org and make every dependency build from source. Nix also asks
  # the user before honouring these on an untrusted flake — answering no simply
  # means building locally, and `cachix use catvert` sets it permanently for
  # those who prefer that.
  nixConfig = {
    extra-substituters = [ "https://catvert.cachix.org" ];
    extra-trusted-public-keys = [
      "catvert.cachix.org-1:R5plivdLnx2WtmZkBryZwUF51Uvl6TJldhFGYOcyPXg="
    ];
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
          # Flat names, not the `xorg.*` set: nixpkgs deprecated the latter and
          # emits a rename warning for each attribute, which a consumer sees on
          # every evaluation of its own configuration. shell.nix already used
          # these.
          libx11
          libxcursor
          libxi
          libxrandr
          libxcb
          vulkan-loader
        ];

        # python3 is not optional: Stylo's build script generates its property
        # tables with Mako and panics without an interpreter on PATH. A
        # nix-shell inherits the user's own python3 and hides that, but the
        # sandbox `nix build` runs in does not.
        nativeDeps = with pkgs; [ pkg-config clang wild cmake python3 ];
        buildDeps = runtimeLibs ++ (with pkgs; [ openssl dbus zstd ]);

        # The Blitz rendering tests assert on real layout geometry — line
        # heights, wrapped rows, table widths — so they need fonts to measure.
        # The sandbox has none, and every such test collapses to a height of
        # zero. Pointing fontconfig at the repository's own fonts is both
        # enough (all 71 pass with these alone) and more hermetic than relying
        # on whatever a developer machine happens to have installed.
        testFontsConf = pkgs.makeFontsConf { fontDirectories = [ ./assets/fonts ]; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "aviary";
          version = "0.2.2";
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

          # The test suite runs in the sandbox, which needs two things a
          # developer shell provides without anyone noticing:
          #
          #   * CA certificates — `reqwest::Client::new()` panics outright with
          #     "No CA certificates were loaded from the system", taking down
          #     four tests that only ever build a request and never send it.
          #   * fonts, for the layout assertions (see `testFontsConf`).
          #
          # HOME is set because fontconfig wants to write a cache and the
          # sandbox's default home is not writable.
          nativeCheckInputs = [ pkgs.cacert ];
          preCheck = ''
            export SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt
            export FONTCONFIG_FILE=${testFontsConf}
            export HOME=$TMPDIR
          '';

          # Three layout tests assert pixel geometry for HTML that asks for
          # `font: 16px Arial` — real Outlook markup, which is the point of
          # them. A full system resolves that name through fontconfig's metric
          # aliases (Liberation Sans, DejaVu); the sandbox has neither the font
          # nor the alias rules, nothing gets shaped, and the measured height
          # collapses to zero. They are not skipped because they are flaky:
          # they pass everywhere a desktop's font configuration exists,
          # including the CI job that runs the full suite inside nix-shell.
          #
          # Removing this needs the sandbox to resolve Arial, not a change to
          # the tests: the markup is what real mail looks like.
          checkFlags = [
            "--skip=ui::blitz_body::tests::minified_html5_namespace_keeps_text_after_breaks"
            "--skip=ui::blitz_body::tests::wrapped_text_rows_expand_before_the_next_row"
            "--skip=ui::blitz_body::tests::nonbreaking_spaces_preserve_outlook_style_indentation"
          ];

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
