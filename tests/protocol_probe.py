#!/usr/bin/env python3
"""Expected-reply matrix for terminal queries, through a real daemon PTY.

Run without arguments for the tests. `--child OUT` is the in-pane prober: it
puts its TTY in raw mode, sends each query followed by DSR 5 (whose `CSI 0 n`
answer marks the end of the reply) and writes the replies as JSON.
"""
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import time
import unittest

QUERIES = {
    "DA1": "\x1b[c",
    "DA2": "\x1b[>c",
    "DA2_PARAMS": "\x1b[>0;0c",
    "XTVERSION": "\x1b[>q",
    "XTGETTCAP_Tc": "\x1bP+q5463\x1b\\",
    "XTGETTCAP_RGB": "\x1bP+q524742\x1b\\",
    "XTGETTCAP_Smulx": "\x1bP+q536d756c78\x1b\\",
    "XTGETTCAP_Setulc": "\x1bP+q536574756c63\x1b\\",
    "XTGETTCAP_Ss": "\x1bP+q5373\x1b\\",
    "XTGETTCAP_Se": "\x1bP+q5365\x1b\\",
    "XTGETTCAP_Ms": "\x1bP+q4d73\x1b\\",
    "XTGETTCAP_unknown": "\x1bP+q7a7a\x1b\\",
    "DECRQSS_SGR": "\x1b[1;4:3;38;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m",
    "DECRQSS_DECSCUSR": "\x1b[5 q\x1bP$q q\x1b\\\x1b[0 q",
    "DECRQSS_unsupported": "\x1bP$qr\x1b\\",
    "DECRQM_2026": "\x1b[?2026$p",
    "DECRQM_2004": "\x1b[?2004$p",
    "KITTY_QUERY": "\x1b[?u",
    "KITTY_PUSH_QUERY_POP": "\x1b[>5u\x1b[?u\x1b[<u",
    "MODIFY_OTHER_KEYS": "\x1b[>4;2m\x1b[?4m\x1b[>4m",
    "CSI14t": "\x1b[14t",
    "CSI16t": "\x1b[16t",
    "CSI18t": "\x1b[18t",
    "OSC11": "\x1b]11;?\x07",
    "OSC52_READ": "\x1b]52;c;?\x07",
    "SYNC_ORDER": "\x1b[?2026h\x1b[1m\x1bP$qm\x1b\\\x1b[c\x1b[?2026l\x1b[0m",
}


def child(out):
    import select
    import termios
    import tty
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    tty.setraw(fd)
    replies = {}
    try:
        for name, query in QUERIES.items():
            os.write(1, (query + "\x1b[5n").encode())
            data = b""
            deadline = time.time() + 3
            while time.time() < deadline and not data.endswith(b"\x1b[0n"):
                ready, _, _ = select.select([fd], [], [], 0.1)
                if ready:
                    data += os.read(fd, 4096)
            replies[name] = data.removesuffix(b"\x1b[0n").decode("latin-1")
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, old)
    Path(out).write_text(json.dumps(replies))
    print("PROBE_DONE")


def hexs(text):
    return text.encode().hex().upper()


def expected(version, osc52_copy):
    major, minor, patch = (int(re.match(r"\d+", part).group()) for part in version.split(".")[:3])
    ms = "\x1bP1+r4d73=" + hexs("\x1b]52;%p1%s;%p2%s\x07") + "\x1b\\" if osc52_copy else "\x1bP0+r4d73\x1b\\"
    return {
        "DA1": "\x1b[?6c",
        "DA2": f"\x1b[>0;{major * 10000 + minor * 100 + patch};1c",
        "DA2_PARAMS": f"\x1b[>0;{major * 10000 + minor * 100 + patch};1c",
        "XTVERSION": f"\x1bP>|crow-term({version})\x1b\\",
        "XTGETTCAP_Tc": "\x1bP1+r5463\x1b\\",
        "XTGETTCAP_RGB": "\x1bP1+r524742\x1b\\",
        "XTGETTCAP_Smulx": "\x1bP1+r536d756c78=" + hexs("\x1b[4:%p1%dm") + "\x1b\\",
        "XTGETTCAP_Setulc": "\x1bP1+r536574756c63="
        + hexs("\x1b[58:2::%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m") + "\x1b\\",
        "XTGETTCAP_Ss": "\x1bP1+r5373=" + hexs("\x1b[%p1%d q") + "\x1b\\",
        "XTGETTCAP_Se": "\x1bP1+r5365=" + hexs("\x1b[2 q") + "\x1b\\",
        "XTGETTCAP_Ms": ms,
        "XTGETTCAP_unknown": "\x1bP0+r7a7a\x1b\\",
        "DECRQSS_SGR": "\x1bP1$r0;1;4:3;38;2;1;2;3m\x1b\\",
        "DECRQSS_DECSCUSR": "\x1bP1$r5 q\x1b\\",
        "DECRQSS_unsupported": "\x1bP0$r\x1b\\",
        "DECRQM_2026": "\x1b[?2026;2$y",
        "DECRQM_2004": "\x1b[?2004;2$y",
        "KITTY_QUERY": "\x1b[?0u",
        "KITTY_PUSH_QUERY_POP": "\x1b[?5u",
        "MODIFY_OTHER_KEYS": "\x1b[>4;2m",
        # No GUI reported cell metrics for this CLI-made pane: no pixel answers.
        "CSI14t": "",
        "CSI16t": "",
        "CSI18t": "\x1b[8;24;80t",
        "OSC11": re.compile(r"\x1b\]11;rgb:[0-9a-f]{4}/[0-9a-f]{4}/[0-9a-f]{4}\x07"),
        "OSC52_READ": "",
        "SYNC_ORDER": "\x1bP1$r0;1m\x1b\\\x1b[?6c",
    }


if __name__ == "__main__" and sys.argv[1:2] == ["--child"]:
    child(sys.argv[2])
    sys.exit(0)

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402


class ProbeHarness(daemon_e2e.DaemonHarness):
    osc52 = None

    def daemon_args(self):
        if self.osc52 is None:
            config = self.runtime / "config.toml"
            config.write_text("")
        else:
            config = self.runtime / "config.toml"
            config.write_text(f'[terminal]\nosc52 = "{self.osc52}"\n')
        return ["--config", str(config)]

    def probe(self):
        out = self.runtime / "replies.json"
        pane = self.rpc("new-session", "probe")["Created"]["pane"]
        self.command("resize-pane", pane, "--rows", 24, "--cols", 80)
        python = shlex.quote(sys.executable)
        self.send(pane, f"{python} {shlex.quote(__file__)} --child {shlex.quote(str(out))}")
        self.eventually(lambda: out.exists() and out.stat().st_size > 0, timeout=60)
        version = subprocess.run([str(daemon_e2e.BIN), "--version"], capture_output=True, text=True,
                                 check=True).stdout.split()[-1]
        return json.loads(out.read_text()), version

    def check(self, osc52_copy):
        replies, version = self.probe()
        for name, want in expected(version, osc52_copy).items():
            with self.subTest(query=name):
                got = replies[name]
                if isinstance(want, re.Pattern):
                    self.assertRegex(got, want)
                else:
                    self.assertEqual(got, want)


@unittest.skipIf(os.name == "nt", "POSIX raw-mode prober")
class DefaultConfigProbe(ProbeHarness):
    def test_expected_replies_with_default_config(self):
        self.check(osc52_copy=True)


@unittest.skipIf(os.name == "nt", "POSIX raw-mode prober")
class Osc52OffProbe(ProbeHarness):
    osc52 = "off"

    def test_expected_replies_with_osc52_off(self):
        self.check(osc52_copy=False)


if __name__ == "__main__":
    unittest.main(verbosity=2)
