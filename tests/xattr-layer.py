# xattr-layer.py <docker-archive (.tar or .tar.gz)> <out.tar>: the same image plus one layer holding
# /app/caps with xattrs (security.capability = cap_net_bind_service, user.flyyy = yes), as PAX
# SCHILY.xattr records. The Nix store can't hold xattrs, so dockerTools can't make such a layer.
import hashlib, io, json, sys, tarfile

src, dst = sys.argv[1:3]
CAP = bytes.fromhex("0100000200040000000000000000000000000000")  # VFS_CAP_REVISION_2, effective, bit 10


def member(t: tarfile.TarFile, name: str) -> bytes:
    f = t.extractfile(name)
    assert f is not None, name
    return f.read()


layer = io.BytesIO()
with tarfile.open(fileobj=layer, mode="w", format=tarfile.PAX_FORMAT) as lt:
    body = b"has xattrs\n"
    ti = tarfile.TarInfo("app/caps")
    ti.size, ti.mode = len(body), 0o755
    # Binary values go through tarfile's surrogateescape (hdrcharset=BINARY).
    ti.pax_headers = {"SCHILY.xattr.security.capability": CAP.decode("utf-8", "surrogateescape"), "SCHILY.xattr.user.flyyy": "yes"}
    lt.addfile(ti, io.BytesIO(body))
layer_bytes = layer.getvalue()
diff_id = "sha256:" + hashlib.sha256(layer_bytes).hexdigest()

with tarfile.open(src) as t, tarfile.open(dst, "w") as out:
    manifest = json.loads(member(t, "manifest.json"))
    assert len(manifest) == 1, "one image per archive"
    config = json.loads(member(t, manifest[0]["Config"]))
    config["rootfs"]["diff_ids"].append(diff_id)
    config.setdefault("history", []).append({"created_by": "tests/xattr-layer.py"})
    config_bytes = json.dumps(config).encode()
    config_name = hashlib.sha256(config_bytes).hexdigest() + ".json"
    replaced = {"manifest.json", manifest[0]["Config"]}
    manifest[0]["Config"] = config_name
    manifest[0]["Layers"].append("xattr/layer.tar")
    for m in t.getmembers():
        if m.name not in replaced:
            out.addfile(m, t.extractfile(m) if m.isfile() else None)
    for name, data in [(config_name, config_bytes), ("xattr/layer.tar", layer_bytes), ("manifest.json", json.dumps(manifest).encode())]:
        ti = tarfile.TarInfo(name)
        ti.size = len(data)
        out.addfile(ti, io.BytesIO(data))
