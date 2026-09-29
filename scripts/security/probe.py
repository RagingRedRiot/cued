#!/usr/bin/env python3
"""Raw access probes: deliberately bypass cued's client and speak the wire
protocol (DESIGN.md §5.1) directly, so the daemon's own check is what is tested.

  probe.py allowed SOCKET JOB        owner: ping, list sees JOB
  probe.py fs-denied SOCKET          foreign: the filesystem refuses the connect
  probe.py uid-denied SOCKET JOB     foreign: connects, but gets no reply to
                                     ping, list, or cancel JOB
  probe.py unreadable PATH...        foreign: every PATH refuses open/listing
"""
import errno
import json
import os
import socket
import sys


def assert_unprivileged():
    # The probe must be exactly the user it claims to be, with nothing that
    # could let it step around file permissions.
    assert os.getuid() == os.geteuid() and os.getgid() == os.getegid()
    status = dict(line.split(':', 1) for line in open('/proc/self/status'))
    assert int(status['CapEff'].strip(), 16) == 0, 'probe holds capabilities'
    assert status['NoNewPrivs'].strip() == '1'


def request(body):
    return (json.dumps({'proto': 1, 'body': body}) + '\n').encode()


def connect(path):
    s = socket.socket(socket.AF_UNIX)
    # A timeout is a failure, never a rejection: a hung or crashed daemon must
    # not pass as one that refused the caller.
    s.settimeout(5)
    s.connect(path)
    return s


def exchange(path, body):
    """Send one request on a fresh connection; return every byte received."""
    s = connect(path)
    received = bytearray()
    try:
        s.sendall(request(body))
        while chunk := s.recv(65536):
            received.extend(chunk)
            assert len(received) < 1 << 20, 'unexpectedly large reply'
            if received.endswith(b'\n'):
                break
    except (BrokenPipeError, ConnectionResetError):
        pass
    finally:
        s.close()
    return bytes(received)


def main():
    assert_unprivileged()
    mode, args = sys.argv[1], sys.argv[2:]

    if mode == 'allowed':
        path, job = args
        pong = json.loads(exchange(path, {'cmd': 'ping'}))
        assert pong == {'result': 'pong', 'proto': 1}, pong
        listed = json.loads(exchange(path, {'cmd': 'list', 'all': True}))
        assert listed['result'] == 'job_list', listed
        ids = [f"j{entry['id']}" for entry in listed['jobs']]
        assert job in ids, f'{job} missing from {ids}'
        print(f'PASS: owner uid {os.getuid()} pinged and listed {job} over the raw socket')

    elif mode == 'fs-denied':
        (path,) = args
        try:
            connect(path).close()
        except PermissionError as exc:
            assert exc.errno == errno.EACCES, exc
            print(f'PASS: uid {os.getuid()} refused by the filesystem at {path}')
            return
        raise AssertionError(f'uid {os.getuid()} connected to {path} through private permissions')

    elif mode == 'uid-denied':
        path, job = args
        # Reachable on purpose: the fixture relaxed the socket's permissions so
        # that this exercises the daemon's peer-credential check, not the mode
        # bits. A read, a liveness check, and a mutation are each refused.
        for body in ({'cmd': 'ping'}, {'cmd': 'list', 'all': True}, {'cmd': 'cancel', 'job': job}):
            received = exchange(path, body)
            assert not received, f'foreign uid got a reply to {body}: {received!r}'
        print(f'PASS: uid {os.getuid()} connected, but ping, list and cancel {job} got no reply')

    elif mode == 'unreadable':
        for path in args:
            try:
                if os.path.isdir(path):
                    os.listdir(path)
                else:
                    open(path, 'rb').close()
            except PermissionError:
                continue
            raise AssertionError(f'uid {os.getuid()} could read {path}')
        print(f'PASS: uid {os.getuid()} cannot read {len(args)} private path(s)')

    else:
        raise SystemExit(f'unknown mode {mode!r}')


if __name__ == '__main__':
    main()
