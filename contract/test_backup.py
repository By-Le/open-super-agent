"""备份：导出 → 上传 → 确认 → 服务自己重启后换上备份里的数据。备份文件 Python 版也解得开（两个版本格式一样）。"""
import base64
import subprocess
import sys
import time
import zipfile

import httpx

from conftest import ROOT, until

PASS = "correct horse battery"


def b64(s: str) -> str:
    return base64.b64encode(s.encode()).decode()


def test_export_restore_restart(srv):
    keep = srv.make_bot("Keepme")
    info = srv.get("/api/backup")
    assert info["counts"]["bots"] >= 2 and info["staged"] is None and info["version"]
    started = info["started"]

    r = srv.http.post("/api/backup/export", json={"passphrase": "short"})
    assert r.status_code == 400 and "口令至少" in r.json()["detail"]
    r = srv.http.post("/api/backup/export", json={"passphrase": PASS})
    assert r.status_code == 200, r.text
    assert r.headers["content-disposition"].startswith('attachment; filename="osa-backup-')
    assert r.headers["x-backup-unreadable"] == "0"
    data = r.content
    assert data.startswith(b"OSABAK1\n")

    # Python 版的 decrypt 解得开（格式一样）
    f = srv.tmp / "b.osabak"
    f.write_bytes(data)
    out = srv.tmp / "b.zip"
    subprocess.run([sys.executable, "-c", "import sys; from osa.backup import decrypt; from pathlib import Path; "
                    "decrypt(Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3])", str(f), str(out), PASS],
                   cwd=ROOT, check=True, env={"OSA_CONFIG": str(srv.tmp / "config.yaml"), "OSA_DATA_DIR": str(srv.tmp / "pydata"),
                                              "PATH": "/usr/bin:/bin"})
    names = zipfile.ZipFile(out).namelist()
    assert {"backup.json", "secrets.json", "osa.db"} <= set(names)

    gone = srv.make_bot("AfterBackup")   # 恢复后应该没有了

    r = srv.http.put("/api/backup/restore", content=data, headers={"x-passphrase": b64("wrong passphrase")})
    assert r.status_code == 400 and "口令不对" in r.json()["detail"]
    r = srv.http.put("/api/backup/restore", content=b"", headers={"x-passphrase": b64(PASS)})
    assert r.status_code == 400
    r = srv.http.put("/api/backup/restore", content=data, headers={"x-passphrase": b64(PASS)})
    assert r.status_code == 200, r.text
    summary = r.json()
    assert summary["counts"]["bots"] == info["counts"]["bots"] and summary["format"] == 1
    assert srv.get("/api/backup")["staged"]["counts"] == summary["counts"]

    # 取消再重新上传
    assert srv.delete("/api/backup/restore") == {"ok": True}
    assert srv.get("/api/backup")["staged"] is None
    r = srv.http.post("/api/backup/restore/confirm")
    assert r.status_code == 400
    r = srv.http.put("/api/backup/restore", content=data, headers={"x-passphrase": b64(PASS)})
    assert r.status_code == 200

    time.sleep(1.1)   # started 精确到秒以下，保证重启后的不一样
    assert srv.post("/api/backup/restore/confirm") == {"restarting": True}

    def back():
        try:
            i = srv.http.get("/api/backup").json()
            return i if i["started"] != started else None
        except (httpx.HTTPError, ValueError):
            return None
    until(back, timeout=40, what="服务重启")
    names = [b["name"] for b in srv.get("/api/bots")["bots"]] if False else [b["name"] for b in srv.get("/api/bootstrap")["bots"]]
    assert "Keepme" in names and "AfterBackup" not in names
    assert srv.proc.poll() is None
    assert keep["id"] and gone["id"]
    olds = list(srv.data.glob("osa.db.before-restore-*"))
    assert olds, "原来的数据库要留一份"
