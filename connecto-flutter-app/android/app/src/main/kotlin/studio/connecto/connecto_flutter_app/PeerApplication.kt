package studio.connecto.connecto_flutter_app

import android.app.Application

class PeerApplication : Application() {
    private external fun initializePeer(): Boolean

    override fun onCreate() {
        super.onCreate()
        System.loadLibrary("connecto_flutter_peer")
        check(initializePeer()) { "Could not initialize the Connecto network context" }
    }
}
