import type { TraceabilityMatrix, TraceabilityMatrixRow } from "@engrams/spec-document";

import "./spec-gap-check.css";

const VERDICT_CLASS = {
  covered: "spec-gap-verdict-covered",
  gap: "spec-gap-verdict-gap",
  scope: "spec-gap-verdict-scope",
} as const;

const VERDICT_MARK = {
  covered: "✓",
  gap: "gap",
  scope: "scope?",
} as const;

/**
 * The traceability matrix (mock 2j). Every requirement is a row, every layer
 * below the requirement ledger is a column, and a cell with nothing behind it
 * is amber. The rows with no requirement are the scope-creep detector: content
 * that cites nothing.
 */
export function SpecTraceabilityMatrix({ matrix }: { matrix: TraceabilityMatrix }) {
  if (matrix.rows.length === 0) {
    return (
      <p className="spec-gap-empty">
        This spec states no requirement to trace. Write the requirements first.
      </p>
    );
  }

  return (
    <div className="spec-gap-matrix-scroll">
      <table className="spec-gap-matrix" aria-label="Traceability matrix">
        <thead>
          <tr>
            <th scope="col">Requirement</th>
            {matrix.layers.map((layer) => (
              <th key={layer.key} scope="col">
                {"→"} {layer.title} coverage
              </th>
            ))}
            <th scope="col">Verdict</th>
          </tr>
        </thead>
        <tbody>
          {matrix.rows.map((row, index) => (
            <MatrixRow key={rowKey(row, index)} row={row} />
          ))}
        </tbody>
      </table>
    </div>
  );
}

function MatrixRow({ row }: { row: TraceabilityMatrixRow }) {
  return (
    <tr>
      <td className="spec-gap-requirement">
        {row.requirementId === null ? (
          <span className="spec-gap-requirement-label">(uncited)</span>
        ) : (
          <>
            <span className="spec-gap-requirement-id">{row.requirementId}</span>{" "}
            <span className="spec-gap-requirement-label">
              {"·"} {row.label}
            </span>
          </>
        )}
      </td>
      {row.cells.map((cell) => (
        <td key={cell.layerKey} className={cell.note === null ? undefined : "spec-gap-cell"}>
          {cell.note !== null
            ? cell.note
            : cell.citations.length > 0
              ? cell.citations.map((citation) => (
                  <span
                    key={`${citation.sectionId}:${citation.blockIndex}`}
                    className="spec-gap-citation"
                  >
                    §{citation.sectionTitle} ¶{citation.blockIndex + 1}
                  </span>
                ))
              : "—"}
        </td>
      ))}
      <td>
        <span className={`spec-gap-verdict ${VERDICT_CLASS[row.verdict]}`}>
          {VERDICT_MARK[row.verdict]}
        </span>
      </td>
    </tr>
  );
}

/** A scope row has no requirement id, so fall back to its position. */
function rowKey(row: TraceabilityMatrixRow, index: number): string {
  return row.requirementId ?? `uncited:${index}`;
}
