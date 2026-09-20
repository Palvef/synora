# Source from a repository wrapper after initializing _here.
prepare_tuna_scope() {
    case "${SYNC_SCOPE_POLICY:-}" in
        '') return 0 ;;
        tuna) ;;
        *) echo 'Unsupported SYNC_SCOPE_POLICY' >&2; return 1 ;;
    esac
    SYNORA_TUNA_SCOPE_FILE=$(mktemp)
    export SYNORA_TUNA_SCOPE_FILE
    trap 'rm -f -- "$SYNORA_TUNA_SCOPE_FILE"' EXIT
    python3 "${_here}/helpers/tuna_scope.py" prepare "$1" > "$SYNORA_TUNA_SCOPE_FILE" || return 1
    python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); print("TUNA scope: repository="+d["repository"]+" commit="+d["source_commit"]); [print("TUNA selection: "+json.dumps(r,sort_keys=True)) for r in d["rules"]]' "$SYNORA_TUNA_SCOPE_FILE"
}
