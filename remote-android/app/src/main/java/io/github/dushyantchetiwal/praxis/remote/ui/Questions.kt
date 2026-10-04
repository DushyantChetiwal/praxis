package io.github.dushyantchetiwal.praxis.remote.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.toggleable
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Card
import androidx.compose.material3.CardDefaults
import androidx.compose.material3.Checkbox
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.RadioButton
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import io.github.dushyantchetiwal.praxis.remote.DeviceUi
import io.github.dushyantchetiwal.praxis.remote.MainViewModel
import io.github.dushyantchetiwal.praxis.remote.R
import io.github.dushyantchetiwal.praxis.remote.data.QuestionHeader
import io.github.dushyantchetiwal.praxis.remote.data.questionAnswerContent

@Composable
fun QuestionCards(ui: DeviceUi, vm: MainViewModel) {
    val summary = ui.currentWindow()?.thread ?: return
    summary.questions.filter { it.key !in ui.answeredQuestions }.forEach { header -> QuestionCard(header, vm) }
    if (summary.questionCount > summary.questions.size) {
        TextButton(onClick = { vm.loadQuestions() }) { Text(stringResource(R.string.questions_all)) }
    }
}

@Composable
private fun QuestionCard(header: QuestionHeader, vm: MainViewModel) {
    Card(colors = CardDefaults.cardColors(containerColor = MaterialTheme.colorScheme.tertiaryContainer), modifier = Modifier.fillMaxWidth()) {
        Column(Modifier.padding(12.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
            Text(stringResource(R.string.question_needed), style = MaterialTheme.typography.labelLarge)
            header.sessionTitle?.let { Text(it, style = MaterialTheme.typography.labelSmall) }
            Text(header.title, maxLines = 3, overflow = TextOverflow.Ellipsis)
            TextButton(onClick = { vm.openQuestion(header) }) { Text(stringResource(R.string.question_open)) }
        }
    }
}

@Composable
fun QuestionDialogs(ui: DeviceUi, vm: MainViewModel) {
    val state = ui.question
    if (state.visible && state.header != null) {
        val form = state.form
        val canAnswer = form != null && questionAnswerContent(form, state.selected, state.freeform) != null
        AlertDialog(
            onDismissRequest = vm::dismissQuestion,
            title = { Text(stringResource(R.string.question_needed)) },
            confirmButton = {
                TextButton(onClick = { vm.submitQuestion() }, enabled = canAnswer && !state.loading && !state.sending) {
                    Text(stringResource(R.string.question_submit))
                }
            },
            dismissButton = {
                Row {
                    TextButton(onClick = { vm.submitQuestion(decline = true) }, enabled = form != null && !state.loading && !state.sending) {
                        Text(stringResource(R.string.question_decline))
                    }
                    TextButton(onClick = vm::dismissQuestion) { Text(stringResource(R.string.action_close)) }
                }
            },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    state.header.sessionTitle?.let { Text(it, style = MaterialTheme.typography.labelSmall) }
                    if (state.loading || state.sending) LoadingRow(stringResource(if (state.sending) R.string.question_sending else R.string.question_loading))
                    state.error?.let {
                        Text(it, color = MaterialTheme.colorScheme.error)
                        TextButton(onClick = { vm.openQuestion(state.header) }, enabled = !state.loading && !state.sending) {
                            Text(stringResource(R.string.action_refresh))
                        }
                    }
                    if (form != null) {
                        LazyColumn(Modifier.heightIn(max = 300.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                            item { Markdown(form.question) }
                            if (form.autoAnswerPaused) item {
                                Text(stringResource(R.string.question_auto_paused), style = MaterialTheme.typography.labelSmall)
                            }
                            items(form.options, key = { it.value }) { option ->
                                val selected = option.value in state.selected
                                val selection = if (form.allowMultiple) Modifier.toggleable(
                                    value = selected, enabled = !state.sending, role = Role.Checkbox,
                                    onValueChange = { vm.selectQuestionOption(option.value) },
                                ) else Modifier.selectable(
                                    selected = selected, enabled = !state.sending, role = Role.RadioButton,
                                    onClick = { vm.selectQuestionOption(option.value) },
                                )
                                Row(Modifier.fillMaxWidth().then(selection).padding(vertical = 4.dp), verticalAlignment = Alignment.CenterVertically) {
                                    if (form.allowMultiple) Checkbox(selected, onCheckedChange = null) else RadioButton(selected, onClick = null)
                                    Spacer(Modifier.width(8.dp))
                                    Column {
                                        Text(option.label)
                                        option.description?.let { Text(it, style = MaterialTheme.typography.bodySmall) }
                                    }
                                }
                            }
                        }
                        OutlinedTextField(
                            value = state.freeform,
                            onValueChange = vm::editQuestionAnswer,
                            label = { Text(stringResource(if (form.options.isEmpty()) R.string.question_answer else R.string.question_freeform)) },
                            enabled = !state.sending,
                            modifier = Modifier.fillMaxWidth(),
                            maxLines = 4,
                        )
                    }
                }
            },
        )
    } else if (ui.questionList.visible) {
        val list = ui.questionList
        AlertDialog(
            onDismissRequest = vm::dismissQuestionList,
            title = { Text(stringResource(R.string.questions_all)) },
            confirmButton = { TextButton(onClick = vm::dismissQuestionList) { Text(stringResource(R.string.action_close)) } },
            text = {
                Column {
                    if (list.loading) LoadingRow(stringResource(R.string.question_loading))
                    list.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
                    TextButton(onClick = { vm.loadQuestions() }, enabled = !list.loading) { Text(stringResource(R.string.action_refresh)) }
                    LazyColumn(Modifier.heightIn(max = 360.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
                        items(list.questions.filter { it.key !in ui.answeredQuestions }, key = { it.key }) { QuestionCard(it, vm) }
                        if (list.nextOffset != null) item {
                            TextButton(onClick = { vm.loadQuestions(more = true) }, enabled = !list.loading) { Text(stringResource(R.string.questions_more)) }
                        }
                    }
                }
            },
        )
    }
}
