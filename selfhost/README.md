# Self-hosted Sectr

Run all of Sectr on your own machine: the journal, secrets, runners and builds. You need Docker with the Compose plugin.

```sh
git clone https://github.com/sectr-dev/sectr && cd sectr/selfhost
./setup.sh && docker compose up -d
```

`setup.sh` prints a platform key. Open <http://localhost:3000> and sign in with it. Then deploy an agent:

```sh
curl -fsSL https://sectr.dev/install.sh | sh
sectr auth login --api http://localhost:8080
sectr init my-agent && cd my-agent && sectr deploy
```

The [self-hosting guide](https://docs.sectr.dev/guides/self-hosting/) covers where to run it, the Docker socket, TLS, settings, upgrades and backups.

To upgrade, change `SECTR_VERSION` in `.env`, then run `docker compose pull && docker compose up -d`.
