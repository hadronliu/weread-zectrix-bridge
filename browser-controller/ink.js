'use strict';

// Runs in the browser. Work on a copy of the screenshot, never the reader DOM.
async function enhanceInk({ pngB64, blackPoint = 200, whitePoint = 250 }) {
  const image = new Image();
  image.src = 'data:image/png;base64,' + pngB64;
  await image.decode();
  const canvas = document.createElement('canvas');
  canvas.width = image.naturalWidth;
  canvas.height = image.naturalHeight;
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = '#fff';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(image, 0, 0);
  const pixels = ctx.getImageData(0, 0, canvas.width, canvas.height);
  for (let i = 0; i < pixels.data.length; i += 4) {
    const gray = 0.2126 * pixels.data[i] + 0.7152 * pixels.data[i + 1] + 0.0722 * pixels.data[i + 2];
    const ink = Math.round(Math.max(0, Math.min(255, (gray - blackPoint) * 255 / (whitePoint - blackPoint))));
    pixels.data[i] = pixels.data[i + 1] = pixels.data[i + 2] = ink;
  }
  ctx.putImageData(pixels, 0, 0);
  return canvas.toDataURL('image/png').split(',')[1];
}

module.exports = { enhanceInk };
