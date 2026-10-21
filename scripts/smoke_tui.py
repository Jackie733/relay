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
    ssh_fixtures = "--ssh-fixtures" in sys.argv[2:]
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
                                   start_new_session=True,
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
    if ssh_fixtures:
        smoke_ssh_pty(binary)


def smoke_ssh_pty(binary):
    fixture_key = Path(__file__).resolve().parent.parent / "tests/integration/.fixtures/id_ed25519"
    with tempfile.TemporaryDirectory(prefix="relay-ssh-smoke-") as directory:
        known_hosts = str(Path(directory) / "known_hosts")
        master, slave = pty.openpty()
        before = termios.tcgetattr(slave)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        process = subprocess.Popen(
            [
                binary,
                "--ssh",
                "127.0.0.1",
                "2221",
                "relay",
                str(fixture_key),
                known_hosts,
                directory,
            ],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            start_new_session=True,
            env={**os.environ, "TERM": "xterm-256color"},
        )
        transcript = bytearray()
        screen = pyte.Screen(100, 30)
        stream = pyte.Stream(screen)
        decoder = codecs.getincrementaldecoder("utf-8")("replace")

        def screen_text():
            return "\n".join(
                "".join(screen.buffer[y][x].data for x in range(screen.columns))
                for y in range(screen.lines)
            )

        def read_for(seconds=0.2):
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

        def expect(text, seconds=8):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                read_for(0.1)
                if text in screen_text() or text.encode("utf-8") in transcript:
                    return
            raise AssertionError(f"Missing {text!r}; screen:\n" + screen_text())

        def expect_screen(text, seconds=8):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                read_for(0.1)
                if text in screen_text():
                    return
            raise AssertionError(f"Missing {text!r} on screen:\n" + screen_text())

        try:
            expect("Type yes:")
            os.write(master, b"yes\n")
            expect_screen("[TERMINAL]")
            os.write(master, b"echo smoke-pty-''ok\r")
            expect_screen("smoke-pty-ok")
            # Resize the outer Unix PTY and verify the remote SSH PTY receives it.
            screen.resize(34, 112)
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 112, 0, 0))
            process.send_signal(signal.SIGWINCH)
            time.sleep(0.15)
            os.write(master, b"stty size\r")
            expect_screen("27 110")
            # F12 returns to Control; 2 switches to Files; 3 + Enter re-focuses PTY.
            os.write(master, b"\x1b[24~")
            expect_screen("[CONTROL]")
            os.write(master, b"2")
            expect_screen("Left Local")
            os.write(master, b"3")
            os.write(master, b"\r")
            expect_screen("[TERMINAL]")
            # Fallback escape key Ctrl+] (0x1d) also returns to Control.
            os.write(master, b"\x1d")
            expect_screen("[CONTROL]")
            os.write(master, b"q")
            assert process.wait(timeout=5) == 0
            read_for(0.1)
            assert termios.tcgetattr(slave) == before, "terminal settings were not restored"
            assert b"\x1b[?2004l" in transcript, "bracketed paste was not disabled"
            assert b"\x1b[?1049l" in transcript, "alternate screen was not restored"
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            os.close(master)
            os.close(slave)
    print("PASS: real Unix PTY -> Relay TUI -> SSH PTY end-to-end session, F12/Ctrl+] escape and cleanup")


if __name__ == "__main__":
    main()
