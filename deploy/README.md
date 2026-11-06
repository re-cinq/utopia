# deploy/

The Helm chart re:cinq runs Utopia with, and one values file per cluster.

`chart/` renders Utopia, its CNPG Postgres, the data volume, the ingress, the network policies and the ExternalSecrets that read credentials from the cluster's Secret Manager. `clusters/<name>.values.yaml` is what Argo CD on that cluster deploys, on every merge to `main`. The convention is described in re-cinq/infrastructure, `docs/deploying.md`.

To ship a new image, change `image.digest` in the cluster's values file and merge.

Render locally:

```sh
helm template utopia deploy/chart -n utopia -f deploy/clusters/n8n-cluster.values.yaml
```
