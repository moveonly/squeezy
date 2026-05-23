function sealed(_: unknown) {
  return undefined;
}

@sealed
export class ButtonController {
  click = (): string => "clicked";
}

export namespace ButtonNamespace {
  export function makeLabel(name: string): string {
    return name.toUpperCase();
  }
}
