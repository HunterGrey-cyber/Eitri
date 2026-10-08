#!/bin/sh
# Records that Codex ran something the project declared: writes its own pid to the file named by $1.
echo "$$" > "$1"
exit 0
