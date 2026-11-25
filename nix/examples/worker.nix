# A worker behind NAT (nuc: x86_64; the laptop: aarch64): it dials the control plane's public address
# (WireGuard, outbound) and runs machines as Cloud Hypervisor VMs (/dev/kvm). Import it with the flake and a
# name: `(import ./nix/examples/worker.nix flyyy "worker1")`.
flyyy: name:
{
  imports = [ flyyy.nixosModules.worker ];
  services.flyyy = {
    controlAddress = "203.0.113.10";
    worker = { enable = true; inherit name; };
  };
  # The agent's VMs are Cloud Hypervisor processes: KVM (kvm-intel/kvm-amd on x86_64, built in on arm64).
  boot.kernelModules = [ "wireguard" ];
}
