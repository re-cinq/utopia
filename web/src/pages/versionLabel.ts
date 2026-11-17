export const versionLabel = (version: string): string =>
  /^\d/.test(version) ? `v${version}` : version;
