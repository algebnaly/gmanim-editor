import gmanim
from math import pi
import math

# We use the new @incremental decorator to tell the engine that this is stateful
@gmanim.incremental
def update_cam(scene_ref, t):
    # This acts incrementally or pure?
    # Actually, the original one used absolute t from 0 to 1!
    distance = 2.0 + t * 5.0
    angle = t * math.pi * 2

    x = math.sin(angle) * distance
    z = math.cos(angle) * distance

    scene_ref.set_camera(
        position=(x, 2.0, 2),
        target=(0, 0, 0),
        up=(0, 1, 0)
    )

@gmanim.scene("incremental_scene")
def incremental_scene(scene):
    scene.set_camera(position=(0.0, 0.0, 3.0), target=(0.0, 0.0, -1.0))

    box = gmanim.Box3D(
        center=(0.0, 0.0, 0.0),
        size=(1.0, 1.0, 1.0),
        color=(0, 255, 100, 255)
    )
    scene.add(box)

    line = gmanim.Line(p0=(0, 0, 0), p1=(1, 1, 0))
    scene.add(line)

    rotate_line = gmanim.Rotate(line, axis=(0, 0, 1), center=(0, 0, 0), frames=120)
    
    # Use UpdateFromFunc which will automatically detect @incremental if it was set
    scene.play(rotate_line)
    scene.play(gmanim.UpdateFromFunc(update_cam, frames=60))
gmanim.registry['gravity_drop'](gmanim.scene.Scene())
