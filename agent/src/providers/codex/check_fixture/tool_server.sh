#!/bin/sh
# The smallest stdio tool server Codex will take: newline-delimited JSON-RPC, one tool. The tool carries no hint
# of being read-only, so only the session's approval mode can keep Codex from asking about it. A call writes this
# process's pid to the file named by $1.
marker=$1

while IFS= read -r line; do
    name=${line#*\"method\":}
    name=${name# }
    name=${name#\"}
    name=${name%%\"*}
    case $name in
    notifications/*) continue ;;
    esac

    # The first "id" of a frame is its own: what a frame carries comes after it.
    id=${line#*\"id\":}
    id=${id%%,*}
    id=${id%%\}*}
    case $name in
    initialize)
        version=${line#*\"protocolVersion\":\"}
        version=${version%%\"*}
        result='{"protocolVersion":"'$version'","capabilities":{"tools":{}},"serverInfo":{"name":"eitri","version":"0"}}'
        ;;
    tools/list)
        result='{"tools":[{"name":"probe","description":"A check of how Codex treats a tool.","inputSchema":{"type":"object","properties":{}}}]}'
        ;;
    tools/call)
        echo "$$" > "$marker"
        result='{"content":[{"type":"text","text":"called"}]}'
        ;;
    *)
        printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"method not found"}}\n' "$id"
        continue
        ;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
