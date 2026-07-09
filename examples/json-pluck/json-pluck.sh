#!/usr/bin/env bash
# Project one field from every object in a JSON array: json-pluck users.json email
set -euo pipefail

jq -r ".[].$2" "$1"
