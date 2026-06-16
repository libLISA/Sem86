package nl.liblisa.sem86

import android.net.Uri
import kotlinx.serialization.Serializable
import androidx.core.net.toUri

@Serializable
class EmulatorConfig(var name: String) {

    var ide0FileUriStr: String? = null
    var ide1FileUriStr: String? = null
    var memoryInMb: Int = 128

    fun getIde0FileUri(): Uri? = ide0FileUriStr?.let { it.toUri() }
    fun getIde1FileUri(): Uri? = ide1FileUriStr?.let { it.toUri() }
}