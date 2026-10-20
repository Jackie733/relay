#!/usr/bin/env python3
"""Exercise the real executable in a Unix PTY and inspect its rendered screen."""

import codecs
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

import pyte


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/relay").resolve())
    for args, code, expected in [
        (["--help"], 0, b"Usage: relay"),
        (["--version"], 0, b"relay 0.1.0-dev"),
        (["--unknown"], 1, b"unknown option"),
        ([], 1, b"interactive terminal required"),
    ]:
        result = subprocess.run([binary, *args], capture_output=True, timeout=5)
        assert result.returncode == code, result
        assert expected in result.stdout + result.stderr, result
        assert b"\x1b[" not in result.stdout + result.stderr, result

    with tempfile.TemporaryDirectory(prefix="relay-smoke-") as directory:
        root = Path(directory)
        (root / "child-folder").mkdir()
        (root / "child-folder" / "nested-only.txt").write_text("relay", encoding="utf-8")
        (root / "sample 中文.txt").write_text("relay", encoding="utf-8")
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        process = subprocess.Popen([binary, directory], stdin=slave, stdout=slave, stderr=slave,
                                   env={**os.environ, "TERM": "xterm-256color"})
        transcript = bytearray()
        screen = pyte.Screen(120, 40)
        stream = pyte.Stream(screen)
        decoder = codecs.getincrementaldecoder("utf-8")("replace")

        def screen_text():
            # Read cells directly: pyte.display can fail on orphaned wide-character
            # stubs during incremental redraws involving CJK filenames.
            return "\n".join(
                "".join(screen.buffer[y][x].data for x in range(screen.columns))
                for y in range(screen.lines)
            )

        def read_for(seconds=0.4):
            output = bytearray()
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                if select.select([master], [], [], max(0, deadline - time.monotonic()))[0]:
                    try:
                        data = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not data:
                        break
                    output.extend(data)
                    stream.feed(decoder.decode(data))
            transcript.extend(output)
            return bytes(output)

        def expect(text, seconds=5):
            output = bytearray()
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                output.extend(read_for(0.1))
                if text.decode("utf-8") in screen_text():
                    return
            raise AssertionError(f"Missing {text!r}; screen:\n" + screen_text())

        def send(keys):
            os.write(master, keys)

        try:
            expect(b"No hosts configured.")
            send(b"2")
            expect(b"child-folder")
            send(b"\r")
            expect(b"nested-only.txt")
            send(b"\x7f")
            expect(b"child-folder")
            send(b"\t")
            read_for()
            send(b"\r")
            expect(b"nested-only.txt")
            send(b"3")
            expect(b"No SSH terminal sessions.")
            send(b"4")
            expect(b"No transfer jobs.")
            send(b"?")
            expect(b"available keys")
            send(b"\x1b")
            read_for()
            screen.resize(5, 30)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 5, 30, 0, 0))
            process.send_signal(signal.SIGWINCH)
            expect(b"resize terminal")
            screen.resize(24, 80)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
            process.send_signal(signal.SIGWINCH)
            expect(b"[CONTROL]")
            send(b"q")
            assert process.wait(timeout=5) == 0
            read_for(0.1)
            assert termios.tcgetattr(slave) == before, "terminal settings were not restored"
            assert b"\x1b[?1049l" in transcript, "alternate screen was not restored"
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            os.close(master)
            os.close(slave)

    print("PASS: help, errors, 4 views, both file panes, directories, help, resize, quit and terminal restoration")


if __name__ == "__main__":
    main()
