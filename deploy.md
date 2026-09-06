# zzzboard deployment record

**Upgrade the GCP account to a full (paid) account before the free trial ends on or about
25 Sep 2026, or the VM is deleted. The e2-micro, its 30 GB standard disk and the static IP
stay in the always-free tier after upgrading.**

Deployed 6 Sep 2026 (pass 1b). Project `<gcp-project-id>`, account
`<gcloud-account>`.

## What exists

| Resource | Value |
|---|---|
| VM | `zzzboard`, `us-east1-b`, e2-micro, Ubuntu 24.04 LTS, 30 GB pd-standard, tags `http-server`, `https-server` |
| Static IP | `zzzboard-ip` (us-east1) = **35.229.16.160** |
| Firewall | `default-allow-http` (tcp:80 → tag `http-server`), `default-allow-https` (tcp:443 → tag `https-server`), both from 0.0.0.0/0 |
| On the VM | repo at `/opt/zzzboard`, data at `/var/lib/zzzboard` (uid 65532), 2 GB swapfile, systemd unit `zzzboard.service` (enabled) |
| Stack | `docker compose` with `app` (image `zzzboard:local`, loopback :8080) and `caddy` (:80/:443) |
| TLS | Let's Encrypt via Caddy, ACME contact abuse@zzzboard.org, certs in `/opt/zzzboard/caddy_data` |

## DNS (Namecheap, BasicDNS) — already set

| Domain | Type | Host | Value | TTL |
|---|---|---|---|---|
| zzzboard.org | A | @ | 35.229.16.160 | Automatic |
| zzzboard.org | CNAME | www | zzzboard.org | 30 min |
| zzzboard.net | A | @ | 35.229.16.160 | Automatic |
| zzzboard.net | CNAME | www | zzzboard.net | 30 min |

The default parking records (URL Redirect on `@`, CNAME `www` → parkingpage.namecheap.com) were
removed from both zones; Namecheap does not allow an A record beside a URL Redirect on the same
host. The locked SPF TXT records were left alone. Caddy serves zzzboard.org and 301-redirects
zzzboard.net, www.zzzboard.org and www.zzzboard.net to https://zzzboard.org, path and query
preserved.

## Commands that were run

From the workstation (gcloud already logged in, project set, Compute API enabled):

```
# firewall rules (the project already had an untagged allow-http-https rule; these are the tagged ones)
gcloud compute firewall-rules create default-allow-http  --network=default --direction=INGRESS --action=ALLOW --rules=tcp:80  --source-ranges=0.0.0.0/0 --target-tags=http-server
gcloud compute firewall-rules create default-allow-https --network=default --direction=INGRESS --action=ALLOW --rules=tcp:443 --source-ranges=0.0.0.0/0 --target-tags=https-server

# static IP, then the VM with it attached
gcloud compute addresses create zzzboard-ip --region=us-east1
gcloud compute addresses describe zzzboard-ip --region=us-east1 --format='value(address)'   # 35.229.16.160
gcloud compute instances create zzzboard --zone=us-east1-b --machine-type=e2-micro \
  --image-family=ubuntu-2404-lts-amd64 --image-project=ubuntu-os-cloud \
  --boot-disk-size=30GB --boot-disk-type=pd-standard \
  --tags=http-server,https-server --address=zzzboard-ip

# build here, ship the image, run deploy.sh on the VM (see ship.sh for the exact steps)
./ship.sh
```

`ship.sh` does, in order: `cargo zigbuild --release --locked --target x86_64-unknown-linux-musl`,
`docker buildx build --platform linux/amd64 -f Dockerfile.ship -t zzzboard:amd64 --load .`,
`docker save zzzboard:amd64 | gzip`, `gcloud compute scp <tar> zzzboard:/tmp/zzzboard-image.tar.gz --zone us-east1-b`,
then:

```
gcloud compute ssh zzzboard --zone us-east1-b --command \
  "sudo env ZZZ_IMAGE=/tmp/zzzboard-image.tar.gz bash -c 'curl -fsSL https://raw.githubusercontent.com/SixSeven-Labs/zzzboard/main/deploy.sh | bash'"
```

Compiling Rust on the VM itself works too (`deploy.sh` without `ZZZ_IMAGE` builds with
`docker compose build`, and it now adds swap first), but an e2-micro has 1 GB of RAM and two
shared vCPUs, so the cross-build path is the one to use.

Verification on the VM:

```
gcloud compute ssh zzzboard --zone us-east1-b --command \
  'systemctl is-enabled zzzboard; systemctl is-active zzzboard; swapon --show; \
   cd /opt/zzzboard && sudo docker compose ps && sudo docker compose logs caddy | grep -E "tls|obtain|error" | tail'
```

Result at deploy time: unit `enabled`/`active`, swap on, both containers `Up`. Caddy obtained
Let's Encrypt certificates for zzzboard.org, www.zzzboard.org and zzzboard.net within a minute
of the A records going live; www.zzzboard.net failed its first attempt (DNS still pointed at the
parking page at that instant) and Caddy logged `will retry ... retrying_in: 60`, which is the
retry-not-crash behaviour expected before DNS is in place. Its second Let's Encrypt attempt hit
a "service busy" 503, so Caddy fell over to ZeroSSL and had the certificate three minutes after
the first failure. No intervention was needed.

## Day-two operations

```
# redeploy after a push (cross-build + ship + deploy.sh):
./ship.sh

# or, on the VM, rebuild from source (slow) / pull config changes only:
sudo /opt/zzzboard/deploy.sh

# logs
gcloud compute ssh zzzboard --zone us-east1-b --command 'cd /opt/zzzboard && sudo docker compose logs -f --tail 50'

# the board data is one file; back it up with plain copy
gcloud compute ssh zzzboard --zone us-east1-b --command 'sudo cat /var/lib/zzzboard/log.jsonl' > backups/log-$(date -u +%Y%m%dT%H%M%SZ).jsonl

# smoke test the live site
./smoke.sh https://zzzboard.org
```

## Costs and limits

- Always-free tier covers one e2-micro in us-east1, 30 GB standard PD, one static IP attached to
  a running instance, and 1 GB/month egress (not to China/Australia). Beyond that egress is billed.
- The VM has 1 GB RAM. The app holds the whole log in memory (roughly log size), so if
  `/var/lib/zzzboard/log.jsonl` approaches a few hundred MB it is time for a bigger machine or a
  bounded index. `_log` grows by one line per request.
- Docker's json-file logs are capped at 3 × 10 MB per container.
