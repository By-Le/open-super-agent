"""备份导出出错时，临时目录（osa-backup-*）不能留下。"""
import os
import tempfile

import pytest

from osa import backup, memory


@pytest.fixture
def made(monkeypatch):
    """记下 osa-backup-* 临时目录。"""
    dirs, real = [], tempfile.mkdtemp

    def mkdtemp(*a, **kw):
        dirs.append(real(*a, **kw))
        return dirs[-1]
    monkeypatch.setattr(tempfile, "mkdtemp", mkdtemp)
    return dirs


def test_bad_passphrase_removes_given_snapshot(made):
    snap = backup.snapshot()
    with pytest.raises(backup.BackupError):
        backup.export("short", snap)
    assert not snap.parent.exists()


def test_failed_snapshot_removes_temp_dir(made, monkeypatch):
    class Broken:
        def commit(self):
            pass

        def backup(self, db):
            raise OSError("disk full")
    monkeypatch.setattr(memory, "conn", Broken())
    with pytest.raises(OSError):
        backup.export("x" * 20)
    assert made and not any(os.path.exists(d) for d in made)
