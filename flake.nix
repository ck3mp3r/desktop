{
  description = "Atuin Desktop - Development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    devenv.url = "github:cachix/devenv";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ {
    nixpkgs,
    flake-parts,
    devenv,
    rust-overlay,
    ...
  }:
    flake-parts.lib.mkFlake {inherit inputs;} {
      imports = [
        devenv.flakeModule
      ];

      systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];

      perSystem = {system, ...}: let
        overlays = [(import rust-overlay)];
        pkgs = import nixpkgs {
          inherit system overlays;
          config.allowUnfree = true;
        };
      in {
        devenv.shells.default = {
          name = "atuin-desktop";

          languages = {
            javascript = {
              enable = true;
              package = pkgs.nodejs_20;
              pnpm = {
                enable = true;
                install.enable = true;
              };
            };

            rust = {
              enable = true;
              channel = "stable";
            };

            typescript = {
              enable = true;
            };
          };

          packages = with pkgs;
            [
              # Rust tooling
              rust-analyzer
              cargo-watch
              cargo-edit
              cargo-outdated

              # Node/JS tooling
              nodePackages.typescript
              nodePackages.prettier

              # Build tools
              pkg-config
              openssl

              # Development utilities
              netcat
              jq
              ripgrep
              fd

              # Python for docs
              python3
              python3Packages.mkdocs
              python3Packages.mkdocs-material
            ]
            # Tauri dependencies (macOS) - Let system frameworks handle SDK
            ++ lib.optionals stdenv.isDarwin [
              # Remove explicit framework references - system provides these
            ]
            # Tauri dependencies (Linux)
            ++ lib.optionals stdenv.isLinux [
              webkitgtk_4_1
              gtk3
              cairo
              gdk-pixbuf
              glib
              dbus
              openssl
              librsvg
              libsoup_3
              libayatana-appindicator
            ];

          env = {
            # Node memory settings for large build
            NODE_OPTIONS = "--max-old-space-size=5120";

            # Rust build settings
            RUST_BACKTRACE = "1";

            # macOS-specific environment
            OBJC_DISABLE_INITIALIZE_FORK_SAFETY =
              if pkgs.stdenv.isDarwin
              then "YES"
              else "";
          };

          enterShell = ''
            echo "🚀 Atuin Desktop Development Environment"
            echo ""
            echo "📦 Tools available:"
            echo "  - Node.js: $(node --version)"
            echo "  - pnpm: $(pnpm --version)"
            echo "  - Rust: $(rustc --version)"
            echo "  - Cargo: $(cargo --version)"
            echo ""
            echo "🔨 Common commands:"
            echo "  - pnpm run dev        # Start Vite dev server"
            echo "  - ./script/dev        # Start Tauri dev mode (recommended)"
            echo "  - pnpm run build      # Build for production"
            echo "  - pnpm run test       # Run tests"
            echo "  - pnpm run test-rs    # Run Rust tests"
            echo ""

            # Ensure pnpm dependencies are installed
            if [ ! -d "node_modules" ]; then
              echo "📦 Installing dependencies..."
              pnpm install
            fi
          '';

          git-hooks.hooks = {
            # Rust
            rustfmt.enable = true;
            clippy.enable = true;

            # TypeScript/JavaScript
            prettier = {
              enable = true;
              excludes = ["pnpm-lock.yaml"];
            };

            # Check TypeScript types
            check-ts = {
              enable = true;
              name = "TypeScript type check";
              entry = "${pkgs.nodePackages.typescript}/bin/tsc --noEmit";
              files = "\\.tsx?$";
              language = "system";
            };
          };

          scripts = {
            dev-tauri.exec = ''
              ./script/dev "$@"
            '';

            build-all.exec = ''
              echo "Building TypeScript..."
              pnpm run build
              echo "Building Tauri app..."
              pnpm run tauri build
            '';

            clean.exec = ''
              echo "Cleaning build artifacts..."
              rm -rf dist/
              rm -rf backend/target/
              rm -rf node_modules/.vite/
              echo "Clean complete!"
            '';

            generate-bindings.exec = ''
              cd backend && cargo test export_bindings
            '';
          };
        };
      };
    };
}
