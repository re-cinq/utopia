-- RC-009: a document has at most one queued extract_document job. The index keeps the check
-- in jobs::enqueue_extraction cheap; the delete removes the repeats queued before it, keeping
-- each document's oldest job so it keeps its place. Running this again changes nothing.

CREATE INDEX IF NOT EXISTS jobs_queued_extraction_document
    ON jobs ((payload->>'document_id'))
 WHERE kind = 'extract_document' AND status = 'queued';

DELETE FROM jobs later
 USING jobs earlier
 WHERE later.kind = 'extract_document' AND later.status = 'queued'
   AND earlier.kind = 'extract_document' AND earlier.status = 'queued'
   AND earlier.payload->>'document_id' = later.payload->>'document_id'
   AND earlier.id < later.id;
