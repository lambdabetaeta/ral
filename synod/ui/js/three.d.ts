// The slice of three.js that armillary.js uses; the vendored build ships no types.
export class Vector3 {
  set(x: number, y: number, z: number): this;
}
export class Euler {
  x: number;
  y: number;
}
export class Object3D {
  position: Vector3;
  rotation: Euler;
  add(...objects: Object3D[]): this;
  lookAt(x: number, y: number, z: number): void;
}
export class Group extends Object3D {}
export class Scene extends Object3D {}
export class AmbientLight extends Object3D {
  constructor(color?: number, intensity?: number);
}
export class DirectionalLight extends Object3D {
  constructor(color?: number, intensity?: number);
}
export class PerspectiveCamera extends Object3D {
  constructor(fov?: number, aspect?: number, near?: number, far?: number);
}
export class BufferGeometry {
  rotateX(radians: number): this;
}
export class TorusGeometry extends BufferGeometry {
  constructor(radius?: number, tube?: number, radialSegments?: number, tubularSegments?: number);
}
export class SphereGeometry extends BufferGeometry {
  constructor(radius?: number, widthSegments?: number, heightSegments?: number);
}
export class CylinderGeometry extends BufferGeometry {
  constructor(radiusTop?: number, radiusBottom?: number, height?: number, radialSegments?: number);
}
export class MeshLambertMaterial {
  constructor(parameters?: { color?: number });
}
export class Mesh extends Object3D {
  constructor(geometry?: BufferGeometry, material?: MeshLambertMaterial);
}
export class WebGLRenderer {
  constructor(parameters?: { antialias?: boolean });
}
