# pierport

## Reference implementation for the pierport protocol UIP

This repository contains reference implementation of the [Pierport Protocol UIP](https://github.com/urbit/UIPs/pull/48), and acts as an intermediate proxy for verifying and cleaning up a pier before it gets imported.

The default configuration is a bit aggressive, most notably, `cram` is being used to verify integrity of the pier, before and after performing cleanup tasks.

### Docker usage

#### Building

You may build pierport inside a docker container. In which case, just do the following:

```
docker build . -t pierport
```

#### Running

You may then run it as follows:

```
docker run -p 4242 --name pierport -it pierport
```

To configure pierport, you may choose to either set specific `PRT_` environment variables (eg.: `-e PRT_PU_VERIFY_CRAM=false`), or bind mount a config toml file to the container using `-v path/to/config.toml:/pierport_cfg.toml`.

To see available configuration environment variables, see [`env_cfg.sh`](scripts/env_cfg.sh) file.

#### Running tests

Once you have a built the image, tagged as `pierport`, you may also run the tests:

```
sh scripts/test.sh
```
