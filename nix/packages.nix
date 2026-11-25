# What gets built: Corrosion (patched, from source), the proxy and CLI crate, the guest init, kernel and initrd.
{ pkgs, lib, ... }:
rec {
  # Not nixpkgs#corrosion: that's corrosion-rs (CMake/Rust), a different project.
  # Built from source because the v1.0.0 release binary's jemalloc assumes 4K pages and aborts
  # on this 16K-page kernel ("<jemalloc>: Unsupported system page size").
  corrosion = let
    src = pkgs.fetchFromGitHub {
      owner = "superfly";
      repo = "corrosion";
      # main HEAD: v1.0.0 is the latest release but main has many replication fixes since.
      rev = "190b842ccdf746b8a87ea1beacdf25cb2a21d9ac";
      hash = "sha256-vJg3GPaqUWBJ1Y3sr0na0j1aIaDREYcbRP0Dg/AzVhk=";
    };
  in pkgs.rustPlatform.buildRustPackage {
    pname = "corrosion";
    version = "1.0.0-unstable-2026-10-27";
    inherit src;
    # Fixes the stage 3 kill -9 divergence: a relay can serve two rows with the same
    # (db_version, seq), and ChunkedChanges dropped the second. superfly/corrosion#596,
    # repro: https://github.com/koxu1996/corrosion-dup-seq-divergence.
    # 0002: sync candidates are sorted (most missing, least recently synced, nearest) with a stable
    # sort over candidates in actor-id order. A node that has just joined ties on every key, so in
    # small clusters its first rounds always went to its lowest-id peers, reachable or not: a
    # catch-up delay of tens of seconds, not a stall (25-36 s unpatched, 11-12 s patched). Shuffling
    # first breaks the ties.
    # shortcut: local patches on main; drop them once they (or equivalent fixes) land upstream.
    patches = [ ./patches/corrosion/0001-chunked-changes-drain-dup-seq.patch ./patches/corrosion/0002-shuffle-sync-candidate-ties.patch ];
    cargoLock = { lockFile = "${src}/Cargo.lock"; allowBuiltinFetchGit = true; };
    cargoBuildFlags = [ "-p" "corrosion" ];
    doCheck = false;
    # jemalloc built for 16K pages also runs on 4K kernels.
    JEMALLOC_SYS_WITH_LG_PAGE = "16";
    # Its .cargo/config.toml links x86_64 with -fuse-ld=mold (and nothing on aarch64).
    nativeBuildInputs = [ pkgs.pkg-config pkgs.clang ] ++ lib.optional pkgs.stdenv.hostPlatform.isx86_64 pkgs.mold;
    buildInputs = [ pkgs.openssl pkgs.sqlite ];
  };
  proxy = pkgs.rustPlatform.buildRustPackage {
    pname = "proxy";
    version = "0.1.0";
    src = ../proxy;
    # corro-client and corro-api-types come from the Corrosion commit above (git, pinned by rev).
    cargoLock = { lockFile = ../proxy/Cargo.lock; allowBuiltinFetchGit = true; };
  };
  # The flyyy CLI alone (packages.flyyy, apps.flyyy): `nix profile install '<flake>#flyyy'` puts just it
  # on the PATH, not the proxy package's other binaries. A copy, so it doesn't keep the whole package.
  flyyy = pkgs.runCommand "flyyy" { meta.mainProgram = "flyyy"; } "install -Dm755 ${proxy}/bin/flyyy $out/bin/flyyy";
  # The guest's whole initrd, PID 1 (guest/): the root overlay, the network, the vsock relays, then the
  # app with its config's env, user and workdir; it reaps and forwards signals. Static (musl): nothing else
  # is in the initrd.
  flyyyInit = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
    pname = "flyyy-init";
    version = "0.1.0";
    src = ../guest;
    cargoLock.lockFile = ../guest/Cargo.lock;
  };
  # The guest kernel: allnoconfig plus guest/kernel.config and this arch's guest/kernel-<arch>.config (what
  # they have, and why), in the format Cloud Hypervisor boots directly: Image on arm64, vmlinux (ELF, PVH
  # entry) on x86_64.
  # Installed as $out/kernel on both, so the agent and the modules don't care which.
  arch = pkgs.stdenv.hostPlatform.linuxArch;
  kernelConfig = pkgs.runCommand "fc-guest-kernel-${arch}.config" { } "cat ${../guest/kernel.config} ${../guest/kernel-${arch}.config} > $out";
  kernel = pkgs.stdenv.mkDerivation {
    pname = "fc-guest-kernel";
    inherit (pkgs.linux) version src;
    nativeBuildInputs = with pkgs; [ bison flex bc perl ] ++ lib.optional (arch == "x86_64") pkgs.elfutils;
    postPatch = "patchShebangs scripts";
    configurePhase = ''
      make ARCH=${arch} allnoconfig KCONFIG_ALLCONFIG=${kernelConfig}
      # allnoconfig silently drops options with unmet deps; fail loudly instead (both fragments' lines).
      grep -v '^#' ${kernelConfig} | grep . > wanted
      while read -r opt; do grep -qx "$opt" .config || { echo "missing: $opt"; exit 1; }; done < wanted
    '';
    buildPhase = "make ARCH=${arch} -j$NIX_BUILD_CORES ${{ arm64 = "Image"; x86_64 = "vmlinux"; }.${arch}}";
    installPhase = "install -D ${{ arm64 = "arch/arm64/boot/Image"; x86_64 = "vmlinux"; }.${arch}} $out/kernel; cp .config $out/config";
    # vmlinux is an ELF: as built, not stripped or patched by the fixup.
    dontStrip = true;
    dontPatchELF = true;
  };
  # The initrd: flyyy-init as /init, and nothing else.
  initrd = pkgs.runCommand "fc-initrd" { nativeBuildInputs = [ pkgs.cpio ]; } ''
    mkdir root
    install -m755 ${flyyyInit}/bin/flyyy-init root/init
    (cd root && find . | cpio -o -H newc -R 0:0) > $out
  '';
}
