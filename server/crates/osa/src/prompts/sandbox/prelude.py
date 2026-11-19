
import ctypes, json, os, sys
spec = json.loads(sys.argv[1])
libc = ctypes.CDLL(None, use_errno=True)
libc.mount.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_ulong, ctypes.c_char_p]
RO, NOSUID, NODEV, NOEXEC, REMOUNT, BIND, REC, PRIVATE = 1, 2, 4, 8, 32, 4096, 16384, 1 << 18
LOCKED = {"nosuid": NOSUID, "nodev": NODEV, "noexec": NOEXEC, "noatime": 1024, "nodiratime": 2048,
          "relatime": 1 << 21, "strictatime": 1 << 24}
def mount(src, dst, fs, flags, data=None):
    enc = lambda s: s.encode() if s is not None else None
    if libc.mount(enc(src), enc(dst), enc(fs), flags, enc(data)) != 0:
        e = ctypes.get_errno()
        raise OSError(e, "mount %s: %s" % (dst, os.strerror(e)))
def under(p, roots):
    return any(p == r or p.startswith(r.rstrip("/") + "/") for r in roots)
mount(None, "/", None, REC | PRIVATE)
devs = ["/dev/" + d for d in ("null", "zero", "full", "random", "urandom", "tty")]
held = {p: os.open(p, os.O_PATH) for p in spec["ro"] + spec["rw"] + devs if os.path.exists(p)}
for h in spec["hide"] + ["/dev"]:
    mount("tmpfs", h, "tmpfs", NOSUID | NODEV, "mode=755")
for p in sorted(held, key=len):
    src = "/proc/self/fd/%d" % held[p]
    if p == spec["cfg"]:
        src = "/proc/self/fd/%d" % held["/dev/null"]
    if os.path.isdir(src):
        os.makedirs(p, exist_ok=True)
    else:
        os.makedirs(os.path.dirname(p), exist_ok=True)
        open(p, "a").close()
    mount(src, p, None, BIND | REC)
os.makedirs("/dev/shm", exist_ok=True)
mount("tmpfs", "/dev/shm", "tmpfs", NOSUID | NODEV, "mode=1777")
for name, target in (("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")):
    os.symlink(target, "/dev/" + name)
rw = spec["rw"] + ["/tmp", "/dev/shm"]
for line in open("/proc/self/mountinfo"):
    f = line.split()
    m = f[4].replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
    if under(m, rw):
        continue
    flags = REMOUNT | BIND | RO
    for o in f[5].split(","):
        flags |= LOCKED.get(o, 0)
    try:
        mount(None, m, None, flags)
    except OSError as e:
        if e.errno != 2 and not under(m, ["/proc", "/sys"]):   # 2：被盖住了，路径已经不在
            raise
mount("tmpfs", "/sys", "tmpfs", NOSUID | NODEV | NOEXEC | RO, "mode=555")   # sysfs 的文件按权限位能写，整个藏起来
try:
    mount("proc", "/proc", "proc", NOSUID | NODEV | NOEXEC | RO)   # 只看得到沙箱里的进程；只读，/proc/sys 写不了
except OSError:
    mount("tmpfs", "/proc", "tmpfs", NOSUID | NODEV | NOEXEC | RO, "mode=555")   # 挂不了新的就整个藏起来
PR_SET_NO_NEW_PRIVS, PR_CAPBSET_DROP, PR_SET_SECUREBITS = 38, 24, 28
libc.prctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong, ctypes.c_ulong]
libc.prctl(PR_SET_SECUREBITS, 0x2f, 0, 0, 0)   # NOROOT | NO_SETUID_FIXUP 并锁住：root 执行程序也拿不回权限
for cap in range(64):
    libc.prctl(PR_CAPBSET_DROP, cap, 0, 0, 0)
if libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "no_new_privs")
class Hdr(ctypes.Structure):
    _fields_ = [("version", ctypes.c_uint32), ("pid", ctypes.c_int)]
class Data(ctypes.Structure):
    _fields_ = [("effective", ctypes.c_uint32), ("permitted", ctypes.c_uint32), ("inheritable", ctypes.c_uint32)]
if libc.capset(ctypes.byref(Hdr(0x20080522, 0)), (Data * 2)()) != 0:
    raise OSError(ctypes.get_errno(), "capset")
os.chdir(spec["cwd"])
os.execv(spec["argv"][0], spec["argv"])
