#!/bin/bash

if [[ ! -z "${SYNORA_S3_ENDPOINT}" ]]; then
	ENDPOINT="--endpoint-url=${SYNORA_S3_ENDPOINT}"
else
	ENDPOINT=""
fi

# see tuna/tunasync-scripts#183
export AWS_EC2_METADATA_DISABLED=true

[[ ! -d "${SYNORA_STORAGE}" ]] && mkdir -p "${SYNORA_STORAGE}"
mkdir -p /tmp/none; cd /tmp/none || exit 1 # enter an empty folder, so the stars in SYNORA_AWS_OPTIONS are not expanded
exec aws --no-sign-request ${ENDPOINT} s3 sync --exact-timestamps ${SYNORA_AWS_OPTIONS} "${SYNORA_UPSTREAM}" "${SYNORA_STORAGE}"

