# deploy/

The Helm chart re:cinq runs Utopia with, and one values file per cluster.

`chart/` renders Utopia, its CNPG Postgres, the data volume, the ingress, the network policies and the ExternalSecrets that read credentials from the cluster's Secret Manager. `clusters/<name>.values.yaml` is what Argo CD on that cluster deploys, on every merge to `re-dev`, the default branch. The convention is described in re-cinq/infrastructure, `docs/deploying.md`.

To ship a new image, change `image.digest` in the cluster's values file and merge.

Render locally:

```sh
helm template utopia deploy/chart -n utopia -f deploy/clusters/internal-cluster.values.yaml
```

## Backups and restore

`database.backup` sends a nightly base backup and continuous WAL to a GCS bucket through the barman-cloud plugin. The cluster's Kubernetes service account `utopia-db` reaches the bucket through Workload Identity; the Google service account and the bucket are defined in re-cinq/infrastructure, `projects/internal/database-backups.tf`.

`database.restore.from` names a server in that bucket to be born from. While `database.restore.replica` is true the cluster replays that server's WAL and serves reads only. Setting it to false promotes the cluster; Utopia then runs its migrations and serves writes. Leave `from` set afterwards: CNPG reads `bootstrap` only when the cluster is created.

## Cutover from n8n-cluster

The old `utopia-db` on n8n-cluster backs up into the same bucket under the name `utopia-db`; the new cluster restores from it and follows it as a replica. The freeze is the few minutes it takes to stop the old app, ship its last WAL, promote and move the data volume:

1. Scale the old deployment to zero and run `SELECT pg_switch_wal()` on the old database so its last WAL segment reaches the bucket.
2. Wait until the new cluster's `status.currentPrimaryTimestamp` LSN matches, then merge `database.restore.replica: false`.
3. Copy the data volume (`/app/data`, files and index) from the old volume to the new one, through the bucket with `gcloud storage rsync`.
4. Delete the old ingress; the old external-dns drops the record and the new one creates it within two minutes.
