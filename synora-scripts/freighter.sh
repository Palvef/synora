#!/bin/bash
set -e
FREIGHTER=${FREIGHTER:-"/usr/local/cargo/bin/freighter-registry"}
CRATES_UPSTREAM="https://static.crates.io/crates"
INDEX_UPSTREAM="https://mirrors.tuna.tsinghua.edu.cn/git/crates.io-index.git"
SYNORA_UPSTREAM=${SYNORA_UPSTREAM:-$CRATES_UPSTREAM}
SYNORA_UPSTREAM=${SYNORA_UPSTREAM%/}
CONF="$SYNORA_STORAGE/config.toml"
INIT=${INIT:-"0"}

if [ ! -d "$SYNORA_STORAGE" ]; then
	mkdir -p $SYNORA_STORAGE
	INIT="1"
elif [ -d "$SYNORA_STORAGE/crates" ]; then
	INIT="1"
fi

echo "Syncing to $SYNORA_STORAGE"

cat > $CONF << EOF
[log]
# see https://docs.rs/log4rs/1.2.0/log4rs/append/file/struct.FileAppenderDeserializer.html#configuration
encoder = "{d}:{l} - {m}{n}"
# unit is MB
limit = 100
level = "info"
[crates]
index_domain = "$INDEX_UPSTREAM"
domain = "$CRATES_UPSTREAM"
download_threads = 16
serve_domains = [
    "localhost",
    ]
[proxy]
enable = false
# git_index_proxy = "127.0.0.1:6780"
# download_proxy = "127.0.0.1:6780"
EOF

if [[ $INIT == "0" ]]; then
	$FREIGHTER -c $SYNORA_STORAGE crates pull
	exec $FREIGHTER -c $SYNORA_STORAGE crates download
else
	$FREIGHTER -c $SYNORA_STORAGE crates pull
	exec $FREIGHTER -c $SYNORA_STORAGE crates download --init
fi
