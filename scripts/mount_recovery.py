"""Called only by test-mount-recovery.sh inside a disposable mount namespace."""
import json
import pathlib
import signal
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def wait_for(check, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if check():
            return
        time.sleep(0.05)
    raise AssertionError("condition did not become true before deadline")


with tempfile.TemporaryDirectory(prefix="logshipper-mount-test-") as temp:
    root = pathlib.Path(temp)
    source, backing, mount = [root / name for name in ("source", "backing", "mount")]
    for path in (source, backing, mount):
        path.mkdir()
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    config = root / "config.toml"
    config.write_text(f'''state_dir = "{root / 'state'}"
settle_seconds = 0
scan_interval_seconds = 1
checkpoint_bytes = 1048576
[destination]
mount_path = "{mount}"
namespace = "test"
require_mount = true
allowed_filesystems = ["tmpfs"]
min_free_bytes = 0
[io]
read_bytes_per_second = 4194304
write_bytes_per_second = 4194304
operations_per_second = 10000
[monitoring]
listen = "127.0.0.1:{port}"
[compression]
format = "zstd"
[[sources]]
name = "app"
path = "{source}"
''')
    (source / "large.log").write_bytes(b"log line\n" * (4 * 1024 * 1024))
    log = (root / "daemon.log").open("w+")
    process = subprocess.Popen([sys.argv[1], "--config", str(config), "--json"], stdout=log, stderr=log)

    def status(path):
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/{path}", timeout=1) as reply:
                return reply.status
        except urllib.error.HTTPError as error:
            return error.code
        except (OSError, urllib.error.URLError):
            return None

    def checkpointed():
        try:
            with sqlite3.connect(root / "state/state.sqlite", timeout=1) as db:
                return db.execute("SELECT count(*) FROM progress WHERE input > 0 AND done=0").fetchone()[0] > 0
        except sqlite3.Error:
            return False

    try:
        wait_for(lambda: status("healthz") == 200)
        assert status("readyz") == 503 and not list(mount.iterdir())
        subprocess.run(["mount", "-t", "tmpfs", "logshipper-test", str(backing)], check=True)
        subprocess.run(["mount", "--bind", str(backing), str(mount)], check=True)
        wait_for(checkpointed)
        subprocess.run(["umount", "-l", str(mount)], check=True)
        wait_for(lambda: status("readyz") == 503)
        assert process.poll() is None and status("healthz") == 200
        assert not list(mount.iterdir()), "daemon wrote into the unmounted directory"
        subprocess.run(["mount", "--bind", str(backing), str(mount)], check=True)
        wait_for(lambda: bool(list(mount.rglob("receipt.json"))), timeout=90)
        receipts = list(mount.rglob("receipt.json"))
        assert len(receipts) == 1
        subprocess.run([sys.argv[1], "--verify", str(receipts[0])], check=True, timeout=30)
        receipt = json.loads(receipts[0].read_text())
        assert receipt["fingerprint"]["size"] == (source / "large.log").stat().st_size
        print("PASS: absent startup, mount, mid-transfer detach, no local fallback, remount, resume, full verification")
    except BaseException:
        log.flush()
        print((root / "daemon.log").read_text(), file=sys.stderr)
        raise
    finally:
        process.send_signal(signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        log.close()
        for path in (mount, backing):
            subprocess.run(["umount", "-l", str(path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
