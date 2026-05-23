export interface RunnerOptions {
  name: string;
}

export function buildRunner(options: RunnerOptions): string {
  return options.name;
}

export const formatRunner = (name: string): string => name.trim();
