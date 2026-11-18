# deploy/

The Helm chart re:cinq runs Utopia with, and one values file per cluster.

`chart/` renders Utopia, its CNPG Postgres, the data volume, the ingress, the network policies and the ExternalSecrets that read credentials from the cluster's Secret Manager. `clusters/<name>.values.yaml` is what Argo CD on that cluster deploys, on every merge to `re-dev`, the default branch. The convention is described in re-cinq/infrastructure, `docs/deploying.md`.

To ship a new image, push a `v*` tag (or run the Release workflow by hand with a tag name): the workflow builds the image, then commits its tag and digest into the cluster's values file on `re-dev`, and Argo CD deploys that commit. The `re.N` at the end of a `v0.1.0-re.N` tag is compiled into the server and is the version the UI shows in the top bar. To roll back, change `image.digest` back by hand and merge.

Render locally:

```sh
helm template utopia deploy/chart -n utopia -f deploy/clusters/internal-cluster.values.yaml
```

## Sign-in with Microsoft Entra

Not yet run against a real Entra tenant. What the code needs from it:

- `oidc.issuer` is `https://login.microsoftonline.com/<tenant id>/v2.0`, with the tenant's GUID and not its domain name: the tokens carry the GUID.
- `oidc.allowedDomains` lists the email domains whose people get an account on first sign-in.
- The app registration is single-tenant, has the redirect URI `https://<hostname>/api/v1/auth/oidc/callback`, and its client id and secret go in the `utopia-oauth` Secret.
- Under Token configuration, the ID token carries the optional claims `email` and `xms_edov`. Entra sends no `email_verified`; `xms_edov` is its statement that the owner of the email's domain is verified, and Utopia opens an account only when it is true.

A person whose token has no `email`, or no true `xms_edov`, is sent back to the login page and gets no account.

## Backups and restore

`database.backup.objectStore` (a barman ObjectStore `configuration`, here GCS) sends a nightly base backup and continuous WAL through the barman-cloud plugin. The cluster's Kubernetes service account `utopia-db` reaches the bucket through Workload Identity; the Google service account and the bucket are defined in re-cinq/infrastructure, `projects/internal/database-backups.tf`.

`database.restore.host` names another CNPG primary to be born from, with `pg_basebackup` over TLS as its `streaming_replica` user. While `database.restore.replica` is true the cluster streams from that primary and serves reads only. Setting it to false promotes the cluster; Utopia then runs its migrations and serves writes. Leave `host` set afterwards: CNPG reads `bootstrap` only when the cluster is created.

## Cutover from n8n-cluster

The old `utopia-db` primary on n8n-cluster is exposed on an internal address only the new cluster's nodes may reach; the new cluster is born from it and follows it as a streaming replica. The freeze is the few minutes it takes to stop the old app, let the replica catch up, promote and move the data volume:

1. Scale the old deployment to zero.
2. Wait until `pg_last_wal_replay_lsn()` on the new cluster equals `pg_current_wal_lsn()` on the old one, then merge `database.restore.replica: false`.
3. Copy the data volume (`/app/data`, files and index) from the old volume to the new one, through the bucket with `gcloud storage rsync`.
4. Delete the old ingress; the old external-dns drops the record and the new one creates it within two minutes.
