import { S } from "./i18n";

/** Where a piece of text came from (0040). Original text is the norm and is not labelled; only text the model read out, transcribed, or described
 *  those get a sentence in the evidence and documents, so a person knows the wording may have misread a character or misheard a name,
 *  or that no one ever actually wrote it that way. */
export type Origin = "stated" | "ocr" | "transcribed" | "described";
export type Anchor = Record<string, unknown> | null;

const num = (v: unknown): number | null =>
  typeof v === "number" && Number.isFinite(v) ? v : null;

/** Writes milliseconds as the recording's clock time: under an hour as m:ss, over an hour as h:mm:ss */
export function clock(ms: number): string {
  const total = Math.floor(ms / 1000);
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  const pad = (n: number) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(s)}` : `${m}:${pad(s)}`;
}

/** A short provenance line: 'OCR · p. 2' or 'Transcribed · 0:06–0:09 · A, B'. Original text returns null */
export function originLabel(origin?: string | null, anchor?: Anchor): string | null {
  switch (origin) {
    case "ocr":
      return S.origin.ocr(num(anchor?.page));
    case "transcribed": {
      const start = num(anchor?.start_ms);
      const end = num(anchor?.end_ms);
      const speakers = Array.isArray(anchor?.speaker)
        ? anchor.speaker.filter((s): s is string => typeof s === "string")
        : typeof anchor?.speaker === "string"
          ? [anchor.speaker]
          : [];
      return S.origin.transcribed(
        start !== null && end !== null ? `${clock(start)}–${clock(end)}` : null,
        speakers,
      );
    }
    case "described":
      return S.origin.described;
    default:
      return null;
  }
}

/** Hover note: what to watch for with this kind of provenance, and which model read it */
export function originHint(origin?: string | null, model?: string | null): string | undefined {
  const why =
    origin === "ocr"
      ? S.origin.ocrHint
      : origin === "transcribed"
        ? S.origin.transcribedHint
        : origin === "described"
          ? S.origin.describedHint
          : null;
  if (!why) return undefined;
  return model ? `${why} ${S.origin.readBy(model)}` : why;
}
