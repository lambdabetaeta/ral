import type { PerspectiveCamera, Scene, WebGLRenderer } from "./three.d.ts";

export class AsciiEffect {
  constructor(renderer: WebGLRenderer, charSet?: string, options?: { resolution?: number; invert?: boolean });
  domElement: HTMLElement;
  setSize(width: number, height: number): void;
  render(scene: Scene, camera: PerspectiveCamera): void;
}
