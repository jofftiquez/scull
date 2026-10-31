export const SCHEMAS_DIR: string;
export const TYPES_PATH: string;
export function loadSchemas(): { stem: string; schema: Record<string, unknown> }[];
export function combinedSchema(): Record<string, unknown>;
export function renderTypes(): Promise<string>;
