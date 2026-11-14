# Mac programs from Linux bash (docs/processes.md, section 7; ROADMAP.md
# item 13's list). Run by the runtime's tests with nixpkgs' arm64 bash,
# with Mac programs on, as
#   bash mac_programs.sh PATH DIR
# PATH has Linux tools only (coreutils, grep, procps); Mac programs are
# named by their paths.
# Prints "mac programs ok" at the end; the first check that fails says so
# and exits 1.
set -u
export PATH=$1
cd "$2" || exit 1
fail() { echo "FAIL: $*"; exit 1; }
want() { [ "$2" = "$3" ] || fail "$1: got '$2', want '$3'"; }

# Programs, exit codes, the environment.
want uname "$(/usr/bin/uname -s)" Darwin
want sw_vers "$(/usr/bin/sw_vers -productName)" macOS
/usr/bin/false; want false $? 1
/usr/bin/true; want true $? 0
want env "$(FOO=bar /usr/bin/env | grep '^FOO=')" FOO=bar
# The Mac's PATH, not the guest's.
/usr/bin/env | grep -q "^PATH=.*/usr/bin" || fail "env: the Mac's PATH"

# Pipes, both ways, and Mac to Mac.
want "linux | mac" "$(echo hello | /usr/bin/tr a-z A-Z)" HELLO
want "mac | linux" "$(/usr/bin/printf 'a\nb\nc\n' | wc -l)" 3
want "mac | mac" "$(/bin/echo x y | /usr/bin/awk '{print $2}')" y

# Exit statuses through wait: codes and signals (Darwin numbers in Linux's).
/bin/sh -c 'exit 7'; want "exit 7" $? 7
/bin/sleep 10 & p=$!; kill -TERM $p; wait $p; want TERM $? 143
/bin/sleep 10 & p=$!; kill -KILL $p; wait $p 2>/dev/null; want KILL $? 137
/bin/sleep 10 & p=$!; kill -USR1 $p; wait $p 2>/dev/null; want USR1 $? 138
/bin/sh -c 'kill -USR2 $$' 2>/dev/null; want "USR2 from itself" $? 140

# fds by guest number, redirects, the working directory.
exec 5>fd5; /bin/sh -c 'echo five >&5'; exec 5>&-
want "fd 5" "$(cat fd5)" five
/bin/echo out >redirected 2>&1; want redirect "$(cat redirected)" out
/bin/sh -c 'echo err >&2' 2>stderr; want stderr "$(cat stderr)" err
want "fd 0" "$(echo in | /bin/cat)" in
mkdir -p sub; (cd sub && /bin/pwd -P >../pwd); want cwd "$(cat pwd)" "$(cd sub && pwd -P)"

# A Mac binary that can't run: execve fails, with its errno (bash: 126).
head -c 20000 /usr/bin/true >badmac; chmod +x badmac
./badmac 2>badmac.err; want "bad Mac binary" $? 126
grep -q "Exec format error" badmac.err || fail "bad Mac binary: $(cat badmac.err)"

# Job control on Mac programs (without a terminal).
set -m
/bin/sleep 10 & p=$!; sleep 0.3
state() { cut -d' ' -f3 /proc/$1/stat; }
kill -STOP $p; sleep 0.3; want "SIGSTOP" "$(state $p)" T
jobs -l | grep -q Stopped || fail "jobs after SIGSTOP: $(jobs -l)"
kill -CONT $p; sleep 0.3; want "SIGCONT" "$(state $p)" S
jobs -l | grep -q Running || fail "jobs after SIGCONT: $(jobs -l)"
kill -TSTP $p; sleep 0.3; want "SIGTSTP" "$(state $p)" T
bg >/dev/null; sleep 0.3; want "bg" "$(state $p)" S
kill -INT $p; wait $p; want "SIGINT" $? 130
set +m

# A stand-in in /proc and ps: its pid, parent, Mac program's name and
# command line.
/bin/sleep 5 & p=$!; sleep 0.3
want "comm" "$(cat /proc/$p/comm)" sleep
want "cmdline" "$(tr '\0' ' ' </proc/$p/cmdline)" "/bin/sleep 5 "
want "ppid" "$(cut -d' ' -f4 /proc/$p/stat)" $$
ps -eo pid=,comm= | grep -q "^ *$p sleep$" || fail "ps: $(ps -eo pid=,comm=)"
kill $p; wait $p

# Mac git with a pager, its output through a pipe.
/usr/bin/git init -q repo && /usr/bin/git -C repo -c user.name=t -c user.email=t@t commit -q --allow-empty -m first ||
    fail "git"
want "git log" "$(PAGER=/usr/bin/less /usr/bin/git -C repo log --format=%s)" first

# The pasteboard: the user's contents put back afterwards.
saved=$(/usr/bin/pbpaste)
echo "macwl $$" | /usr/bin/pbcopy
want "pbcopy | pbpaste" "$(/usr/bin/pbpaste)" "macwl $$"
printf %s "$saved" | /usr/bin/pbcopy

# A Mac program of a Mac program (sh starting ls), and many in a row.
want "sh -c ls" "$(/bin/sh -c '/bin/ls -d sub')" sub
for i in $(seq 1 50); do /usr/bin/true || fail "true $i"; done

echo "mac programs ok"
