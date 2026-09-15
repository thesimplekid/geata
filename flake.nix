{
  description = "Geata: a simple reverse proxy with automatic HTTPS, built on Pingora";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/6713828a351efa628b025a1adf7f43cbf8597513";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      eachSystem = f: nixpkgs.lib.genAttrs systems (system: f (import nixpkgs { inherit system; }));
      project =
        pkgs:
        let
          toolchain = (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;
          craneLib =
            assert pkgs.rustc.version == toolchain;
            crane.mkLib pkgs;
          common = {
            pname = "geata";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
            src = craneLib.cleanCargoSource ./.;
            strictDeps = true;
            OPENSSL_NO_VENDOR = "1";
            nativeBuildInputs = [
              pkgs.pkg-config
              pkgs.cmake
            ];
            buildInputs = [ pkgs.openssl ];
          };
          cargoArtifacts = craneLib.buildDepsOnly common;
          args = common // {
            inherit cargoArtifacts;
          };
        in
        rec {
          package = craneLib.buildPackage args;
          checks = {
            format = craneLib.cargoFmt common;
            nix-format = pkgs.runCommand "geata-nix-format" { } ''
              ${pkgs.nixfmt}/bin/nixfmt --check ${./flake.nix}
              touch $out
            '';
            clippy = craneLib.cargoClippy (args // { cargoClippyExtraArgs = "--all-targets -- -D warnings"; });
            tests = craneLib.cargoTest args;
            demo =
              pkgs.runCommand "geata-demo-check"
                {
                  nativeBuildInputs = [
                    pkgs.python3
                    pkgs.nodejs
                  ];
                }
                ''
                  python3 ${./examples/traffic-demo}/check.py ${package}/bin/geata
                  node ${./examples/traffic-demo}/check-ui.cjs
                  touch $out
                '';
            integration =
              pkgs.runCommand "geata-integration"
                {
                  nativeBuildInputs = [
                    pkgs.python3
                    pkgs.openssl
                    pkgs.curl
                  ];
                }
                ''
                  python3 ${./tests}/smoke.py ${package}/bin/geata --pebble-bin ${pkgs.pebble}/bin
                  touch $out
                '';
          };
        };
    in
    {
      packages = eachSystem (pkgs: {
        default = (project pkgs).package;
      });
      checks = eachSystem (
        pkgs: (project pkgs).checks // { build = self.packages.${pkgs.stdenv.hostPlatform.system}.default; }
      );
      formatter = eachSystem (pkgs: pkgs.nixfmt);
      devShells = eachSystem (pkgs: {
        default = pkgs.mkShell {
          OPENSSL_NO_VENDOR = "1";
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            nixfmt
            pkg-config
            cmake
            openssl
            just
            python3
            nodejs
            curl
            pebble
          ];
        };
      });
    };
}
