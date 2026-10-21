{
  description = "manage.nix: a control plane for fleets of Nix-managed machines";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});

      rustFor =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;
          sqlFilter = path: _type: lib.hasSuffix ".sql" path;
          src = lib.cleanSourceWith {
            src = ./.;
            name = "manage-nix-source";
            filter = path: type: (sqlFilter path type) || (craneLib.filterCargoSources path type);
          };
          commonArgs = {
            inherit src;
            pname = "manage-nix";
            version = (lib.importTOML ./Cargo.toml).workspace.package.version;
            strictDeps = true;
            # Queries are checked against the committed `.sqlx/` data, never a
            # live database.
            SQLX_OFFLINE = "true";
          };
          cargoArtifacts = craneLib.buildDepsOnly commonArgs;
          buildBin =
            crate: bin:
            craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = bin;
                cargoExtraArgs = "--locked -p ${crate}";
                # Tests run once for the whole workspace in `checks.cargo-test`.
                doCheck = false;
                meta.mainProgram = bin;
              }
            );
        in
        {
          inherit
            craneLib
            commonArgs
            cargoArtifacts
            src
            ;
          packages = {
            mnix-agent = buildBin "mnix-agent" "mnix-agent";
            mnix-server = buildBin "mnix-server" "mnix-server";
            mnix = buildBin "mnix-cli" "mnix";
          };
        };
    in
    {
      packages = forAllSystems (
        _system: pkgs:
        let
          inherit (rustFor pkgs) packages;
        in
        packages // { default = packages.mnix; }
      );

      nixosModules = {
        agent = import ./nix/modules/agent.nix self;
        server = import ./nix/modules/server.nix self;
      };

      checks = forAllSystems (
        system: pkgs:
        let
          rust = rustFor pkgs;
          inherit (rust) craneLib commonArgs cargoArtifacts;
        in
        rust.packages
        // {
          cargo-test = craneLib.cargoTest (commonArgs // { inherit cargoArtifacts; });

          cargo-clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- --deny warnings";
            }
          );

          cargo-fmt = craneLib.cargoFmt { inherit (commonArgs) src pname version; };

          # The store suite against a throwaway Postgres server. SQLite runs in
          # `cargo-test`.
          store-postgres = craneLib.cargoTest (
            commonArgs
            // {
              inherit cargoArtifacts;
              pname = "store-postgres";
              cargoTestExtraArgs = "-p mnix-store";
              nativeCheckInputs = [ pkgs.postgresql ];
              MNIX_TEST_REQUIRE_POSTGRES = "1";
              preCheck = ''
                export PGDATA="$(mktemp -d)/data"
                # Unix socket paths are limited to ~104 bytes; keep it short.
                export PGHOST="$(mktemp -d /tmp/pg.XXXXXX)"
                initdb --username=postgres --auth=trust --no-sync >/dev/null
                pg_ctl --wait --log="$PGDATA/server.log" \
                  --options="-c listen_addresses= -c unix_socket_directories=$PGHOST -c fsync=off" \
                  start
                export MNIX_TEST_POSTGRES_URL="postgres://postgres@localhost/postgres?host=$PGHOST"
              '';
              postCheck = ''
                pg_ctl --wait --mode=immediate stop
                rm -rf "$PGHOST"
              '';
            }
          );
        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          vm-hello = import ./nix/tests/hello.nix { inherit self pkgs; };
          vm-activate = import ./nix/tests/activate.nix { inherit self pkgs; };
        }
      );

      devShells = forAllSystems (
        _system: pkgs: {
          default = (rustFor pkgs).craneLib.devShell {
            checks = { };
            packages = with pkgs; [
              rust-analyzer
              sqlx-cli
              postgresql
              attic-client
              nodejs
            ];
            SQLX_OFFLINE = "true";
          };
        }
      );

      formatter = forAllSystems (_system: pkgs: pkgs.nixfmt-tree);
    };
}
