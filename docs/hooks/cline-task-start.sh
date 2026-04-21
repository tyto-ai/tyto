#!/bin/bash
# Place at .clinerules/hooks/TaskStart.sh
# Injects session memories when a new Cline task starts.
tyto inject --type session --project "$(basename "$PWD")"
