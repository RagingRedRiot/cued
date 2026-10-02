# Records docs/assets/demo.gif with VHS (https://github.com/charmbracelet/vhs).
# Re-record after changing cued's output, from the repository root, with a
# static build of cued (the release workflow's, or any musl build):
#
#   docker run --rm -v "$PWD":/vhs -v /path/to/cued:/usr/local/bin/cued:ro \
#     ghcr.io/charmbracelet/vhs docs/demo.tape
#
# Everything runs in the container, against a throwaway home directory.

Output docs/assets/demo.gif

Set Shell bash
Set FontSize 16
Set Width 1200
Set Height 450
Set Padding 24
Set Theme "GitHub Dark"
Set TypingSpeed 35ms

# Two stand-in scripts for the chain, and a daemon started off camera so its
# first-start notice isn't part of the demo.
Hide
Type `export HOME=$(mktemp -d) PS1='$ ' && cd "$HOME"`
Enter
Type `printf '#!/bin/sh\nsleep 1\necho "build ok"\n' > build.sh`
Enter
Type `printf '#!/bin/sh\nsleep 1\necho "tests ok"\n' > test.sh`
Enter
Type `chmod +x build.sh test.sh && cued list >/dev/null 2>&1; clear`
Enter
Sleep 1s
Show

Type `cued remind "25m" "stretch"`
Sleep 400ms
Enter
Sleep 1.5s

Type `cued every "day 09:00" -- ./backup.sh`
Sleep 400ms
Enter
Sleep 1.5s

Type `cued chain ./build.sh --then ./test.sh --wait`
Sleep 400ms
Enter
Sleep 3.5s

Type `cued list`
Sleep 400ms
Enter
Sleep 3s

Type `cued logs j3`
Sleep 400ms
Enter
Sleep 4s
