#!/bin/bash

thread=${SYNORA_LFTP_CONCURRENT:-"5"}
opts=${SYNORA_LFTP_OPTIONS:-""}


[ ! -d "${SYNORA_STORAGE}" ] && mkdir -p "${SYNORA_STORAGE}"
cd ${SYNORA_STORAGE}
lftp "${SYNORA_UPSTREAM}" -e "mirror --verbose --skip-noaccess -P ${thread} --delete ${opts} ; bye"
