# boots: only boot() (the mesh join of four nodes), for looping: `nix build --rebuild` it to shake out rare
# boot or join failures (it found the shared-cgroup collisions behind the one join stall, tests/lib.nix).
ok("booted: all four nodes joined the mesh")
