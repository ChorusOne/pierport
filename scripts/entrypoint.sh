#!/bin/sh

if [ ! -f pierport_cfg.toml ]; then
    ./env_cfg.sh pierport_cfg.toml
fi

pierport -c pierport_cfg.toml
