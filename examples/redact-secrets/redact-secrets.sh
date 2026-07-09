#!/usr/bin/env bash
# Mask token=/password=/secret= values in a log stream, keeping the key.
set -euo pipefail

sed -E 's/(token|password|secret)=\S+/\1=***/gI'
