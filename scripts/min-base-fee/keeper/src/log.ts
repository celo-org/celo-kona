// JSON lines that Cloud Logging parses into structured entries with a severity.
export function info(message: string, fields?: Record<string, unknown>): void {
  console.log(JSON.stringify({ severity: 'INFO', message, ...fields }));
}

export function error(message: string): void {
  console.error(JSON.stringify({ severity: 'ERROR', message }));
}
