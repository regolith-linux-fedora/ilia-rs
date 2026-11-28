"""A D-Bus-activatable application stand-in on the test's private bus.

Run as a script: ``python3 -m parity.fakes.bus_app NAME TRANSCRIPT``. It owns
the well-known name NAME, serves org.freedesktop.Application at the object
path the Desktop Entry spec derives from it, and appends one line per call
to TRANSCRIPT: the method and the object path it was called on, and for
``ActivateAction`` the action's name. It prints
``ready`` once the name is owned.
"""

import sys

from gi.repository import Gio, GLib

INTERFACE = """
<node>
  <interface name="org.freedesktop.Application">
    <method name="Activate"><arg type="a{sv}" name="platform_data" direction="in"/></method>
    <method name="Open">
      <arg type="as" name="uris" direction="in"/>
      <arg type="a{sv}" name="platform_data" direction="in"/>
    </method>
    <method name="ActivateAction">
      <arg type="s" name="action_name" direction="in"/>
      <arg type="av" name="parameter" direction="in"/>
      <arg type="a{sv}" name="platform_data" direction="in"/>
    </method>
  </interface>
</node>
"""


def object_path(name):
    return "/" + name.replace(".", "/").replace("-", "_")


def main(name, transcript):
    loop = GLib.MainLoop()
    info = Gio.DBusNodeInfo.new_for_xml(INTERFACE).interfaces[0]

    def on_call(connection, sender, path, interface, method, parameters, invocation):
        line = f"{method} {path}"
        if method == "ActivateAction":
            line += f" {parameters.unpack()[0]}"
        with open(transcript, "a") as stream:
            stream.write(line + "\n")
        invocation.return_value(None)

    def on_bus(connection, _name):
        connection.register_object(object_path(name), info, on_call, None, None)

    def on_name(_connection, _name):
        print("ready", flush=True)

    def on_lost(_connection, _name):
        loop.quit()

    Gio.bus_own_name(Gio.BusType.SESSION, name, Gio.BusNameOwnerFlags.NONE, on_bus, on_name, on_lost)
    loop.run()


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])
