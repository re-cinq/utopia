/** The endpoint address for push sources.
 *
 * Each of the two push source kinds has its own endpoint: `api` takes documents at `/ingest`, `statements` takes statements at
 * `/statements` (0054). The server returns 404 for the wrong door every time; the token dialog used to show the same for both
 * `/ingest`, so anyone pushing statements from the dialog gets a bare Not found (#924). */
export function pushEndpoint(origin: string, sourceId: string, kind: string): string {
  const door = kind === "statements" ? "statements" : "ingest";
  return `${origin}/api/v1/sources/${sourceId}/${door}`;
}
