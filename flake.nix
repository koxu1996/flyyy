{
  description = "Spike: Cloud Hypervisor VMs (read-only EROFS image + scratch, overlayfs in the guest) inside systemd-nspawn workers";

  inputs = {
    # Latest nixos-unstable (the host is on 3ec6363, so more is built locally).
    nixpkgs.url = "github:NixOS/nixpkgs/31ecbaca1bdefa45dffc409f0250bd8039ea0010";
  };

  outputs = { nixpkgs, ... }:
    let
      inherit (nixpkgs) lib;
      systems = [ "aarch64-linux" "x86_64-linux" ];
      # A role's module with flyyy's packages for one system.
      withPackages = packages: role: {
        imports = [ ./nix/modules/${role}.nix ];
        services.flyyy.packages = lib.mkDefault { inherit (packages) proxy corrosion kernel initrd; };
      };
      # Everything for one system; the outputs below map it over both.
      forSystem = system: let
        pkgs = nixpkgs.legacyPackages.${system};
        # What gets built (nix/packages.nix), the roles' NixOS modules (nix/modules/, the same the
        # nixosModules output exports) with this system's packages, the single-host dev apps (nix/apps.nix),
        # and the rootless apps for real hosts (nix/join.nix, nix/host-apps.nix).
        packages = import ./nix/packages.nix { inherit pkgs lib; };
        roles = lib.genAttrs [ "control" "dashboard" "edge" "worker" ] (withPackages packages);
        fl = import ./nix/modules/lib.nix { inherit pkgs lib packages; };
        inherit (import ./nix/apps.nix ({ inherit pkgs lib; } // packages // fl)) controlApp workerApp edgeApp;
        # `nix run .#join -- --name workerN '<token>'`: a worker without the NixOS module (the dashboard's
        # join dialog shows the command).
        joinApp = import ./nix/join.nix ({ inherit pkgs lib; } // packages // fl);
        # `nix run .#control`, `.#edge`, `.#dashboard`: the control host's roles without the modules.
        inherit (import ./nix/host-apps.nix ({ inherit pkgs lib; } // packages // fl)) controlHostApp edgeHostApp dashboardHostApp;
        inherit (packages) corrosion proxy flyyy flyyyInit kernel initrd;
        testArgs = { inherit pkgs lib roles proxy flyyy joinApp; roleApps = [ controlApp workerApp edgeApp ]; hostApps = [ controlHostApp edgeHostApp dashboardHostApp ]; };
        tests = import ./tests testArgs;
        # The dashboard's shell highlighter (shHighlight), in node.
        dashboard-js = pkgs.runCommand "dashboard-js" { nativeBuildInputs = [ pkgs.nodejs ]; } ''
          node ${./tests/dashboard-js.mjs} ${./proxy/src/dashboard.html} && touch $out
        '';
        # The Rust crate's own checks: clippy with warnings as errors, rustfmt, and its unit tests (the
        # package's checkPhase runs `cargo test`).
        rustChecks = {
          clippy = proxy.overrideAttrs (o: {
            pname = "proxy-clippy";
            nativeBuildInputs = o.nativeBuildInputs ++ [ pkgs.clippy ];
            buildPhase = "cargo clippy --all-targets --offline -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
          });
          fmt = pkgs.runCommand "proxy-fmt" { nativeBuildInputs = [ pkgs.cargo pkgs.rustfmt ]; } ''
            cd ${./proxy} && cargo fmt --check && touch $out
          '';
          cargo-test = proxy;
          # The guest's PID 1 (flyyy-init, static musl): its unit tests, and clippy for the musl target it's
          # built for (ioctl's request type differs from glibc's there).
          guest-test = flyyyInit.overrideAttrs { doCheck = true; };
          guest-clippy = flyyyInit.overrideAttrs (o: {
            pname = "flyyy-init-clippy";
            nativeBuildInputs = o.nativeBuildInputs ++ [ pkgs.pkgsStatic.buildPackages.clippy ];
            buildPhase = "cargo clippy --all-targets --offline --target ${pkgs.pkgsStatic.stdenv.hostPlatform.rust.rustcTarget} -- -D warnings";
            doCheck = false;
            installPhase = "touch $out";
          });
        };
      in
      {
        # NixOS tests on nspawn containers (no QEMU, no sudo): nix build .#checks.<system>.<name>
        # And clippy, fmt, cargo-test: the Rust crate's; dashboard-js: the dashboard's highlighter.
        checks = tests // rustChecks // { inherit dashboard-js; };
        # The test cluster as a library, for tests kept outside the repo (compat/, gitignored).
        legacyPackages.testLib = import ./tests/lib.nix testArgs;
        # The dev stack (replaces up.sh/down.sh): the `stack` test's interactive driver. nspawn outside the
        # build sandbox needs root, hence sudo; run `test_script()` in its REPL to boot and seed.
        apps = {
          stack = {
            type = "app";
            program = "${pkgs.writeShellScript "stack" ''exec sudo ${tests.stack.driverInteractive}/bin/nixos-test-driver "$@"''}";
          };
          control = { type = "app"; program = lib.getExe controlHostApp; };
          edge = { type = "app"; program = lib.getExe edgeHostApp; };
          dashboard = { type = "app"; program = lib.getExe dashboardHostApp; };
          join = { type = "app"; program = lib.getExe joinApp; };
          flyyy = { type = "app"; program = lib.getExe flyyy; };
          local-control = { type = "app"; program = lib.getExe controlApp; };
          local-worker = { type = "app"; program = lib.getExe workerApp; };
          local-edge = { type = "app"; program = lib.getExe edgeApp; };
        };
        packages = {
          inherit kernel initrd proxy flyyy corrosion flyyyInit;
          flyyy-control = controlHostApp;
          flyyy-edge = edgeHostApp;
          flyyy-dashboard = dashboardHostApp;
          flyyy-join = joinApp;
          flyyy-local-control = controlApp;
          flyyy-local-worker = workerApp;
          flyyy-local-edge = edgeApp;
        };
      };
      perSystem = lib.genAttrs systems forSystem;
    in
    # checks.<system>, apps.<system>, ...: each output of forSystem, for both systems.
    lib.genAttrs [ "checks" "legacyPackages" "apps" "packages" ] (out: lib.mapAttrs (_: o: o.${out}) perSystem) // {
      # The roles for real hosts: services.flyyy.{control,dashboard,edge,worker} (nix/modules/; docs/DESIGN.md has a setup),
      # each with flyyy's packages for the host's system.
      nixosModules = lib.genAttrs [ "control" "dashboard" "edge" "worker" ] (role: { pkgs, ... }: withPackages perSystem.${pkgs.stdenv.hostPlatform.system}.packages role);
    };
}
