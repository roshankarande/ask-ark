{
  description = "Ask Ark — unified terminal client for Outlook and Microsoft Teams";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      lib = pkgs.lib;

      # Only the source Cargo needs — never copy target/, .git, .env, logs.
      src = lib.cleanSourceWith {
        src = ./.;
        filter =
          path: type:
          let
            base = baseNameOf path;
          in
          base != "target"
          && base != ".git"
          && base != ".direnv"
          && base != ".env"
          && !(lib.hasSuffix ".log" base);
      };

      common = {
        version = "0.1.0";
        inherit src;
        cargoLock.lockFile = ./Cargo.lock;
        nativeBuildInputs = [ pkgs.pkg-config ];
        # reqwest uses rustls, so no OpenSSL is needed at build or runtime.
        doCheck = false;
      };

      mkBin =
        { pname, cratePkg, description, mainProgram }:
        pkgs.rustPlatform.buildRustPackage (
          common
          // {
            inherit pname;
            cargoBuildFlags = [
              "-p"
              cratePkg
            ];
            meta = {
              inherit description mainProgram;
              license = lib.licenses.mit;
              platforms = lib.platforms.linux;
            };
          }
        );
    in
    {
      packages.${system} = rec {
        ark = mkBin {
          pname = "ask-ark";
          cratePkg = "ask-ark";
          description = "Unified TUI for Outlook and Microsoft Teams";
          mainProgram = "ark";
        };

        m365-webhook = mkBin {
          pname = "m365-webhook";
          cratePkg = "webhook";
          description = "Microsoft Graph change-notification receiver for ask-ark";
          mainProgram = "m365-webhook";
        };

        default = ark;
      };

      # `nix develop` gives a working Rust toolchain (nixpkgs, not the broken
      # rustup one on this host).
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          rustfmt
          clippy
          # Editors pick rust-analyzer up from PATH. Supplying it here keeps it on
          # the same toolchain as the compiler: a rust-analyzer that cannot reach
          # a working rustc reports errors that `cargo check` does not, because it
          # has no sysroot to resolve `std` against.
          rust-analyzer
          pkg-config
        ];
        RUST_BACKTRACE = "1";
      };

      formatter.${system} = pkgs.nixfmt-rfc-style;
    };
}
