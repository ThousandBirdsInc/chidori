import {Rect, Vec2, AffineTransform} from '@/speedscope/lib/math'
import {Color} from '@/speedscope/lib/color'
import {Graphics} from '@/speedscope/gl/graphics'
import {setUniformAffineTransform} from '@/speedscope/gl/utils'

const vertexFormat = new Graphics.VertexFormat()
vertexFormat.add('configSpacePos', Graphics.AttributeType.FLOAT, 2)
vertexFormat.add('color', Graphics.AttributeType.FLOAT, 3)

const vert = `
  uniform mat3 configSpaceToNDC;

  attribute vec2 configSpacePos;
  attribute vec3 color;
  varying vec3 vColor;

  void main() {
    vColor = color;
    vec2 position = (configSpaceToNDC * vec3(configSpacePos, 1)).xy;
    gl_Position = vec4(position, 1, 1);
  }
`

const frag = `
  precision mediump float;
  varying vec3 vColor;

  void main() {
    gl_FragColor = vec4(vColor.rgb, 1);
  }
`


export class NodeBatch {
  private squares: {origin: Vec2, size: number}[] = []
  private colors: Color[] = []
  constructor(private gl: Graphics.Context) {}

  getSquareCount() {
    return this.squares.length
  }

  private buffer: Graphics.VertexBuffer | null = null
  getBuffer(): Graphics.VertexBuffer {
    if (this.buffer) {
      return this.buffer
    }

    const corners = [
      [0, 0],
      [1, 0],
      [0, 1],
      [1, 0],
      [0, 1],
      [1, 1],
    ]

    const bytes = new Uint8Array(vertexFormat.stride * corners.length * this.squares.length)
    const floats = new Float32Array(bytes.buffer)
    let idx = 0

    for (let i = 0; i < this.squares.length; i++) {
      const {origin, size} = this.squares[i]
      const color = this.colors[i]

      for (let corner of corners) {
        floats[idx++] = origin.x + corner[0] * size
        floats[idx++] = origin.y + corner[1] * size

        floats[idx++] = color.r
        floats[idx++] = color.g
        floats[idx++] = color.b
      }
    }

    if (idx !== floats.length) {
      throw new Error("Buffer expected to be full but wasn't")
    }

    this.buffer = this.gl.createVertexBuffer(bytes.length)
    this.buffer.upload(bytes)
    return this.buffer
  }

  addSquare(origin: Vec2, size: number, color: Color) {
    this.squares.push({origin, size})
    this.colors.push(color)

    if (this.buffer) {
      this.buffer.free()
      this.buffer = null
    }
  }

  free() {
    if (this.buffer) {
      this.buffer.free()
      this.buffer = null
    }
  }
}

export interface NodeBatchRendererProps {
  batch: NodeBatch
  configSpaceSrcRect: Rect
  physicalSpaceDstRect: Rect
}

export class NodeBatchRenderer {
  material: Graphics.Material
  constructor(private gl: Graphics.Context) {
    this.material = gl.createMaterial(vertexFormat, vert, frag)
  }

  render(props: NodeBatchRendererProps) {
    setUniformAffineTransform(
      this.material,
      'configSpaceToNDC',
      (() => {
        const configToPhysical = AffineTransform.betweenRects(
          props.configSpaceSrcRect,
          props.physicalSpaceDstRect,
        )

        const viewportSize = new Vec2(this.gl.viewport.width, this.gl.viewport.height)

        const physicalToNDC = AffineTransform.withTranslation(new Vec2(-1, 1)).times(
          AffineTransform.withScale(new Vec2(2, -2).dividedByPointwise(viewportSize)),
        )

        return physicalToNDC.times(configToPhysical)
      })(),
    )

    this.gl.setUnpremultipliedBlendState()
    this.gl.draw(Graphics.Primitive.TRIANGLES, this.material, props.batch.getBuffer())
  }
}
