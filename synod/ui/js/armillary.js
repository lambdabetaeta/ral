// @ts-types="./three.d.ts"
import {
  AmbientLight,
  CylinderGeometry,
  DirectionalLight,
  Group,
  Mesh,
  MeshLambertMaterial,
  PerspectiveCamera,
  Scene,
  SphereGeometry,
  TorusGeometry,
  WebGLRenderer,
} from "../vendor/three/three.module.min.js";
// @ts-types="./ascii-effect.d.ts"
import { AsciiEffect } from "../vendor/three/AsciiEffect.js";

// An armillary sphere drawn in glyphs; the figure is a pure function of the clock.

const COLS = 16;
const ROWS = 7;
// AsciiEffect: glyphs per pixel; its font is 2 / RESOLUTION px.
const RESOLUTION = 0.25;
const TAU = 2 * Math.PI;
const OBLIQUITY = (23.4 * Math.PI) / 180;
// Ring thickness over radius; thinner rings fall between the glyph samples.
const TUBE = 0.09;
const PHI = (1 + Math.sqrt(5)) / 2;
// Turns per second; the golden ratio keeps the rings from ever realigning.
const MERIDIAN_RATE = 1 / 34;
const ECLIPTIC_RATE = -1 / (34 * PHI);

export const armillaryEl = document.createElement("div");
armillaryEl.className = "armillary";
armillaryEl.setAttribute("aria-hidden", "true");

function build() {
  const material = new MeshLambertMaterial({ color: 0xffffff });
  /** @param {number} radius @param {number} inclination */
  const ring = (radius, inclination = 0) => {
    const mesh = new Mesh(new TorusGeometry(radius, TUBE * radius, 12, 128).rotateX(Math.PI / 2), material);
    mesh.rotation.x = inclination;
    return mesh;
  };
  // The groups turn about the polar axis. The equator stays put: turning it would show nothing.
  const meridian = new Group().add(ring(0.86, Math.PI / 2));
  const ecliptic = new Group().add(ring(0.72, OBLIQUITY));

  const sun = new DirectionalLight(0xffffff, 2.6);
  sun.position.set(-2, 3, 4);

  const scene = new Scene();
  scene.add(
    ring(1),
    meridian,
    ecliptic,
    new Mesh(new SphereGeometry(0.2, 16, 12), material),
    new Mesh(new CylinderGeometry(0.04, 0.04, 2.3, 8), material),
    new AmbientLight(0xffffff, 0.6),
    sun,
  );

  const camera = new PerspectiveCamera(24, COLS / (2 * ROWS), 0.1, 20);
  camera.position.set(0, 1.6, 5.2);
  camera.lookAt(0, 0, 0);

  const effect = new AsciiEffect(new WebGLRenderer({ antialias: true }), " .:-=+*#%@", {
    resolution: RESOLUTION,
    invert: true,
  });
  effect.setSize(COLS / RESOLUTION, (2 * ROWS) / RESOLUTION);
  armillaryEl.append(effect.domElement);

  return { effect, scene, camera, meridian, ecliptic };
}

/** @typedef {ReturnType<typeof build>} Stage */
/** @type {Stage | undefined} */
let stage;
let frame = 0;

/** @param {Stage} parts @param {number} ms */
function draw({ effect, scene, camera, meridian, ecliptic }, ms) {
  const seconds = ms / 1000;
  meridian.rotation.y = TAU * MERIDIAN_RATE * seconds;
  ecliptic.rotation.y = TAU * ECLIPTIC_RATE * seconds;
  effect.render(scene, camera);
}

export function startArmillary() {
  if (frame) return;
  try {
    stage ??= build();
  } catch (err) {
    // No WebGL: the status bar still says what is happening.
    console.warn("armillary unavailable:", err);
    return;
  }
  const live = stage;
  draw(live, performance.now());
  if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
  const tick = (/** @type {number} */ ms) => {
    draw(live, ms);
    frame = requestAnimationFrame(tick);
  };
  frame = requestAnimationFrame(tick);
}

export function stopArmillary() {
  cancelAnimationFrame(frame);
  frame = 0;
}
