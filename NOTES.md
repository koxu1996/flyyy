# How flyyy was built

flyyy is a load balancer on Corrosion that grew into a small platform, with a control plane, edges and workers running Firecracker microVMs, over a WireGuard mesh. I built it over about a week. Most of that time an AI agent was doing the work I asked for and running the full test suite, stress tests included. My own hands-on part, thinking, deciding and steering, was about a dozen hours.

Building the test harness first paid off quickly: a storm test found a Corrosion bug where a node that's behind can miss an update for good ([superfly/corrosion#596](https://github.com/superfly/corrosion/issues/596)). flyyy carries the fix as a patch, plus a second one that makes a new node catch up faster in small clusters (from 25-36 s to 11-12 s).

Below is how it was built, stage by stage, and what it would take to go to production. The design in detail is in [docs/DESIGN.md](docs/DESIGN.md).

## Preface: AI alone won't build this

Models have improved a lot. When I started playing with the first ones about four years ago (Copilot, ChatGPT), finishing a line of code was already nice. I watched that progress over the years, and only recently have they become usable enough to build something that makes sense. But we're still not at the level of a "dark software factory", running with no human in the loop: someone who supervises it, steers it, and is the system's main architect.

As a test, I let Fable build this system from a one-page spec on its own, with Kimi K3 (Moonshot) as the reviewer. I'd had good results with Kimi lately: despite the price, I liked the quality of its code. What came out was something completely different from what I had in mind. The reviewer found things to fix, Claude fixed them, and I ran several rounds: review, fix, review, fix. I even tried a pattern with three models: a prosecutor that attacked the code, a defender that defended it, and a judge that decided which way to change it. Two rounds of that "court".

By the fifth or sixth version, with the AI correcting itself, I felt it wasn't moving towards a better design. It was looping on its own idea of what the system should look like. It had already made decisions I didn't agree with, and it wasn't leading anywhere.

So: neither one-shot nor over a longer run can AI on its own build a project like this at the level of one crafted by a human, by a good architect.

## Part 1: the skeleton, in ten prompts

The first part was ten big prompts, one per stage. They're in [docs/history/](docs/history/), and what each stage found is in [docs/history/stages.md](docs/history/stages.md).

### Before stage 1: Corrosion, the stack, the hardware

Before implementing anything, I looked at what Corrosion gives me. It's replicated, eventually consistent SQLite, which is invaluable for this task: many nodes, a system made of many machines, and Corrosion does the synchronization. Otherwise I'd have to model it myself.

Then the architecture and the stack. My laptop is a MacBook running Linux, so not Darwin but Linux on ARM (aarch64), not the usual x86_64. I expected that could cause problems in development, or when deploying the demo to a "demo production".

What I needed: Corrosion for synchronization, and a way to run what users bring: containers, and not necessarily Docker. There are many runtimes, but for security and boot time I took inspiration from Fly.io, which uses Firecracker. Earlier, at Gevulot, I used the Nanos unikernel: we ran arbitrary proving programs on it in our Cosmos-based blockchain. It had lots of problems: processes couldn't use paths like `/tmp`, had no network, and so on, because it's a very isolated system. I hadn't used Firecracker before, but I'd wanted to for a long time; I know Claude uses Firecracker micro-VMs for its sessions and agents.

So the first idea, and I think an important one: run a container image in a Firecracker micro-VM. And all of it in Nix, because I'm a Nix evangelist. Reproducing, testing locally and deploying: done right in Nix, it works everywhere, is 100% reproducible, and avoids a whole class of bugs, deployment time and differences between environments.

### Stage 1: feasibility spike

My first prompt was a feasibility check: can Nix on my laptop run containers (systemd-nspawn workers), and each of them spawn Firecracker micro-VMs, with the root disk made from a Docker image? It was research: can it be done, and what does it need from the host.

Result: yes, it needs KVM. My remote machine is itself virtualized and has no KVM, but the MacBook (aarch64) has virtualization. 3 workers x 2 VMs of `traefik/whoami` worked. Feasible, approved: we go with Firecracker micro-VMs.

### Stage 2: a proxy routing by Host

Then building blocks on top. Like on Fly.io, the app runs in a Firecracker micro-VM, so traffic needs to get from an address to the right VM. That's routing, not load balancing yet: by Host (the subdomain), to that app's VMs. I asked the AI for the next component: a proxy on Tokio and Hyper. Simple: no TLS, plain round robin, just Host -> the right VMs.

Result: worked, no surprises.

### Stage 3: routes from Corrosion, the storm test and a Corrosion bug

Next, routing state moved from static config into Corrosion, since that's the point of the project. Also a feasibility check: does it work on my platform (aarch64; Corrosion's release binary crashes on the 16K-page kernel, so it's built from source), and does it really sync? On paper it's eventually consistent, but in practice? So I did a storm, chaos testing: lots of changes, kills and lag, and check that all nodes end in the same state.

To my surprise, they didn't. A big surprise at the very beginning of the project: Corrosion has a bug. A node that falls behind (after a restart, or lag) can end up with two rows with the same `(db_version, seq)` on the peer it syncs from, and that peer then stops sending at the duplicate, so the change is never synced. The divergence is permanent.

The AI research went towards that bug. I reported it upstream with a deterministic repro: [superfly/corrosion#596](https://github.com/superfly/corrosion/issues/596). Grzegorz (gorbak25) confirmed it's a serious correctness issue, and that with Corrosion's V2 metadata it doesn't occur. Until then, flyyy's flake applies our patch to Corrosion.

To be honest, after switching to Corrosion it took me a long time to get it stable in the stress tests. But with the patch in place and the issue reported, we could move on.

### Stage 4: edges and SO_REUSEPORT

Next I wanted to model what Fly.io has: edges with anycast, one IP served in many regions, connecting to the workers and their VMs. To model it, I added an edge component that routes, and reaches a VM through its worker's backhaul with HTTP CONNECT: also just a simple proxy. Two edges ran locally with `SO_REUSEPORT`: two processes on one port, with the kernel spreading connections between them.

So I had a simulation of the target architecture, still local, but I could already play with it.

Result: worked, routing kept working.

### Stage 5: agents on the workers

Then agents: on each worker, an agent replaces the statically defined VMs. The desired state is in Corrosion, and each agent follows its own worker's rows: "this machine has to run, so I start it". It reports back whether the machine is running. VMs stop being static and become dynamic.

Result: no problems.

### Stage 6: control plane with an API and a CLI

With desired state in Corrosion and agents starting VMs from it, the next step was a control plane: the central component that writes the desired state, deciding which machines should exist. It has an HTTP API, and a CLI (`flyyy`) on top to talk to it easily.

Result: no problems, but the list of workers was still static.

### Stage 7: workers register themselves

So in stage 7, workers register themselves in Corrosion ("this is my name, this is my address") and send a heartbeat. No more static list of workers: it's in Corrosion, and the edges read workers' addresses from that table too.

Result: no problems either.

### Stage 8: EROFS images with an overlay

Stage 8 improved the images. Each worker exports the image (`crane export`) and builds a read-only EROFS filesystem from it (`mkfs.erofs`). That makes building the VM disk easier: before, the root disk was a containerd devmapper snapshot, an ext4 disk of a fixed size, and EROFS just sizes itself from the content, so there's no size to guess or resize. Over the read-only image, every boot gets a fresh scratch drive, stacked with overlayfs. The guest kernel gained EROFS and overlayfs for that.

Result: everything worked. containerd, devmapper and ext4 are gone, and the image is read-only, which is better for security too.

### Stage 9: vsock instead of networking

Stage 9 improved isolation. Until then each Firecracker micro-VM had its own IP address, so the whole networking stack was in it. To cut complexity and improve security, VMs now have no network at all, only a vsock. The worker's backhaul reaches the VM through Firecracker's vsock socket (its `CONNECT <port>` handshake), and inside the guest a small forwarder passes it to the app on localhost. The guest gets simpler: no networking, just a vsock.

Result: worked as it should. Better isolation.

### Stage 10: a guest launcher in Rust

By then I'd run the first compatibility test: 61 popular Docker images, and only 22 ran. Most failures came from how the app was started: the init was a busybox shell script, and the image's Entrypoint, Cmd and Env went through the kernel command line. Hard to maintain and full of hacks. So in stage 10, the last of the written prompts, it became a small static launcher in Rust (`flyyy-init`) with a JSON config per VM: argv, env, working directory and user, the hostname from the machine id. It stays PID 1, reaps zombies and forwards signals.

That gave the initial skeleton of the system, what I'd call version 0.1. The compatibility retest went from 22 to 30 of 61, and showed what to fix next in the guest and the kernel: memory for some images, readiness timeouts, cgroups, IPv6, file capabilities, the clock. It was the first real check, and the first stable version.

## Part 2: iterating on a working prototype

The second part isn't in docs/history's big prompts. It was direct work with the AI: telling it what I didn't like and what should be better, while playing with a system that already ran. All of it is in the conversation linked from [LLM_TRANSCRIPT.md](LLM_TRANSCRIPT.md). For example:
- the edge -> worker traffic went from plain HTTP CONNECT over the LAN, one connection per request, to CONNECT streams over long-lived HTTP/2 connections, inside mTLS, over a WireGuard mesh;
- health checks, and balancing by load;
- desired state moved out of gossip: the control plane keeps it and sends each worker its machines, and Corrosion only carries published facts (service discovery);
- the dashboard and its UI, traffic metrics, app events and logs;
- users and organizations.

The v0.1 prototype, those ten big prompts, built the skeleton I wanted. Because the skeleton was stable, the AI could handle developing it further from there.

## Shortcuts, and the road to production

There were many shortcuts and simplifications along the way, which would need work on the way to production.

- **TLS keys on the edges.** For simplicity, every edge has the certificate together with the domain's private key. That's not great for security. It probably needs keyless TLS: the key stays on another system, which signs handshakes for the edges without ever handing out the private key. That's an important one.
- **Observability.** Metrics are kept in memory on each node for an hour. Going to production we'd want Prometheus, to watch the state of everything and have better oversight.
- **Load balancing and scale.** Each edge picks the less loaded of two random VMs, by its own count of requests in flight. It's not global load balancing; it could be better, with better traffic logging. And honestly, I have no idea how the WireGuard mesh behaves at scale, with 100, 200, 300 or 500 workers: every node is a peer of every other.
- **Deploying from CI.** Today you deploy an image from a registry, pinned by digest. Someone with a Rust project has to build and push an image themselves first. It should work like `fly.toml`: a config file in the repo, and a GitHub Action that builds and deploys from it. Plus a stable, documented API for people who want their own integrations, and billing.
- **Updating a running app.** Today a running app can't change: a new image (or env, services, size) is refused, and the way to a new version is destroy and deploy again, with downtime in between. Production needs releases: rolling or blue/green deployments, where the new machines have to pass their health checks before traffic moves to them, and the old ones go only after that, with a rollback if the new ones fail.
- **Connection draining.** Before a machine stops, its services are marked down and the agent waits until the edges stop sending it new requests (or gives up after 10 s). But connections that are still open are cut when the VM stops: a long request, a websocket or a tcp connection just ends. Stopping also ends the VM from the outside, so the app gets no signal at all. Real draining would send the app a SIGTERM inside the VM first, so it can finish what it's doing, and wait for the open connections, with a grace period. Updates need this too, or every rollout drops requests.
- **Hardening everything:** the kernel, even more focus on isolation, and turning on security features so nobody can escape to another VM or another worker.
- **Let's Encrypt integration.** I got the wildcard certificate from Let's Encrypt by hand, and copied it to every edge. A production system needs an ACME client that issues certificates automatically, monitors their expiry, renews them, and rolls the new ones out to every edge without dropping connections (edges already reload a certificate on SIGHUP, so the rollout is the missing part). Also per-app certificates, for custom domains: today one wildcard covers every app.
