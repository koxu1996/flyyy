# systemd units, the VMs' user units, and the logs.
def start(m, unit: str) -> None:
    """Start a unit and wait until it's active. In the build sandbox the containers share one cgroup
    hierarchy: another container stopping its same-named unit removes this one's cgroup, and the first
    start fails ("Failed to spawn executor"); Restart= brings it up a second later."""
    m.execute(f"systemctl start {shlex.quote(unit)}")
    m.wait_for_unit(unit)


def kill9(m, unit: str) -> None:
    """SIGKILL a unit's main process. Not `systemctl kill`: in the build sandbox the containers share one
    cgroup hierarchy, so it would kill the same-named unit in every container."""
    m.succeed(f"kill -9 $(systemctl show -P MainPID {shlex.quote(unit)})")


# The worker's agent runs as flyyy; each VM is a unit of flyyy's systemd user instance, fc-<id>.
STATE, RUN = "/var/lib/flyyy", "/run/flyyy"
AS_FLYYY = "runuser -u flyyy -- env XDG_RUNTIME_DIR=/run/user/2000"


def uctl(m, args: str) -> tuple[int, str]:
    """systemctl --user of flyyy on worker m."""
    return m.execute(f"{AS_FLYYY} systemctl --user {args}")


def vm_pid(m, mid: str) -> str:
    return uctl(m, f"show -P MainPID fc-{mid}.service")[1].strip()


def vm_journal(m, mid: str) -> str:
    """A VM's console (its Cloud Hypervisor's serial output): a user unit's journal."""
    return m.succeed(f"journalctl _SYSTEMD_USER_UNIT=fc-{shlex.quote(mid)}.service -o cat --no-pager")


# A log line of ours (tracing): `<LEVEL> <message> <key=value fields>`.
LEVEL = re.compile(r"(?m)^ *(?:TRACE|DEBUG|INFO|WARN|ERROR) ")


def journal(m, unit: str, since: float = 0) -> str:
    """A unit's log lines, each from its message on (the level dropped), so a check can anchor on it."""
    # The exact time: a whole second would let in lines from up to a second before it.
    s = f"--since @{since:.6f}" if since else ""
    return LEVEL.sub("", m.succeed(f"journalctl -u {shlex.quote(unit)} {s} -o cat --no-pager"))
